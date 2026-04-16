//! Append-only operation journal.
//!
//! The journal is an immutable sequence of entries; undo/redo is just
//! cursor movement with ref restoration. Because entries are never
//! deleted, every past state is reachable — redo does not disappear
//! when the user does a new operation after undoing.
//!
//! On-disk format: `<repo>/.git/mergefox/journal.jsonl` — one JSON
//! object per line so we can stream-append without rewriting the file.
//!
//! See `src/app.rs` → `design fluent undo/redo` comments for UX rules.

mod entry;
mod snapshot;

pub use entry::{EntryId, JournalEntry, OpSource, Operation, RepoSnapshot};
pub use snapshot::{capture, restore};

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

/// What kind of journal navigation a `JournalNavTask` represents.
///
/// Carried alongside the task so the foreground completion handler knows
/// how to update the cursor once the background work succeeds (we never
/// touch the cursor before the git work commits — if the worker fails,
/// the cursor stays put so the user can retry without diverging state).
#[derive(Debug, Clone, Copy)]
pub enum JournalNavKind {
    Undo,
    Redo,
    /// Restore to the state *before* a specific entry. Used by the panic
    /// recovery flow ("take me back to <entry>"). After restore, cursor
    /// lands one entry earlier so a subsequent redo brings the entry back.
    RestoreToBefore {
        entry_id: EntryId,
    },
}

/// Background-thread async undo / redo / restore.
///
/// Why a custom task type and not the existing `GitJob`?
/// ----------------------------------------------------
/// `GitJob` is shaped around network ops (fetch / push / pull) with their
/// own progress callbacks and credential plumbing. Journal nav is a
/// strictly local sequence (auto-stash → ref restore → force checkout)
/// with no remote, no credentials, and no streaming progress, so reusing
/// `GitJob`'s machinery would just add ceremony.
///
/// Why background at all?
/// ----------------------
/// On big-binary repos (game engines, ML datasets, design assets) a single
/// undo can spend several seconds inside `auto_stash` reading dirty files
/// into git objects, then more seconds writing the previous HEAD's blobs
/// back during force-checkout. Doing that on the egui update thread pins
/// the UI; doing it on a worker keeps the window responsive (the user can
/// cancel by closing the tab, switch to another tab, etc.).
pub struct JournalNavTask {
    pub kind: JournalNavKind,
    /// HUD label shown on success ("Undo: Commit fix login").
    pub label: String,
    pub started_at: Instant,
    rx: mpsc::Receiver<std::result::Result<(), String>>,
}

impl JournalNavTask {
    /// Spawn the worker. The thread receives the repo path and performs
    /// the auto-stash + ref restore via git CLI — no repo handle needed.
    pub fn spawn(
        repo_path: PathBuf,
        target_snapshot: RepoSnapshot,
        reason: String,
        kind: JournalNavKind,
        label: String,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result: std::result::Result<(), String> = (|| -> Result<()> {
                let outcome = crate::git::repo::auto_stash_repository(
                    &repo_path,
                    &reason,
                    crate::git::repo::AutoStashOpts::default(),
                )?;
                if let crate::git::repo::AutoStashOutcome::Refused { reason } = outcome {
                    anyhow::bail!(reason.to_string());
                }
                crate::journal::restore(&repo_path, &target_snapshot)?;
                Ok(())
            })()
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        Self {
            kind,
            label,
            started_at: Instant::now(),
            rx,
        }
    }

    pub fn poll(&self) -> Option<std::result::Result<(), String>> {
        self.rx.try_recv().ok()
    }
}

pub struct Journal {
    /// `.git/mergefox/` directory.
    dir: PathBuf,
    /// Append-only entries, index = position.
    pub entries: Vec<JournalEntry>,
    /// Index of the entry whose AFTER state is the current live state.
    /// `None` = we're at the "void" before any recorded operation.
    pub cursor: Option<usize>,
    next_id: EntryId,
}

impl Journal {
    /// Load (or create) the journal for a repo. `git_dir` = the `.git`
    /// directory, not the workdir.
    pub fn load_or_init(git_dir: &Path) -> Result<Self> {
        let dir = git_dir.join("mergefox");
        fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;

        // Make sure git never tracks our metadata.
        ensure_excluded(git_dir).ok();

        let path = dir.join("journal.jsonl");
        let mut entries: Vec<JournalEntry> = Vec::new();
        let mut next_id: EntryId = 1;
        if path.exists() {
            let f = fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
            for line in BufReader::new(f).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<JournalEntry>(&line) {
                    Ok(e) => {
                        next_id = next_id.max(e.id.saturating_add(1));
                        entries.push(e);
                    }
                    Err(_) => continue, // skip malformed lines rather than bail
                }
            }
        }

        // Read cursor file (just an id on a single line)
        let cursor_file = dir.join("cursor");
        let cursor = fs::read_to_string(&cursor_file)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .and_then(|id| entries.iter().position(|e| e.id == id));

        Ok(Self {
            dir,
            entries,
            cursor,
            next_id,
        })
    }

    pub fn can_undo(&self) -> bool {
        match self.cursor {
            Some(i) => i > 0 || i == 0, // cursor at 0 means we can step to "void"
            None => false,
        }
    }

    pub fn can_redo(&self) -> bool {
        match self.cursor {
            Some(i) => i + 1 < self.entries.len(),
            None => !self.entries.is_empty(),
        }
    }

    /// Append a new operation; `before` and `after` are snapshots taken by
    /// the caller. Returns the new entry id. Any future entries (past the
    /// cursor) are preserved in the log but no longer reachable via redo —
    /// see `history_branches` for UI visualization.
    pub fn record(
        &mut self,
        op: Operation,
        before: RepoSnapshot,
        after: RepoSnapshot,
        source: OpSource,
    ) -> Result<EntryId> {
        let id = self.next_id;
        let entry = JournalEntry {
            id,
            timestamp_unix: now_unix(),
            operation: op,
            before,
            after,
            source,
        };
        append_line(&self.journal_path(), &entry)?;
        self.entries.push(entry);
        self.cursor = Some(self.entries.len() - 1);
        self.next_id = self.next_id.saturating_add(1);
        self.save_cursor()?;
        Ok(id)
    }

    /// Step the cursor back one. Returns the snapshot that should be
    /// restored on the repo (BEFORE state of the entry we're stepping off).
    pub fn step_back(&mut self) -> Option<&RepoSnapshot> {
        let i = self.cursor?;
        let entry = &self.entries[i];
        let snap = &entry.before;
        self.cursor = if i == 0 { None } else { Some(i - 1) };
        let _ = self.save_cursor();
        Some(snap)
    }

    /// Step the cursor forward one. Returns the AFTER state of the entry
    /// we're stepping onto.
    pub fn step_forward(&mut self) -> Option<&RepoSnapshot> {
        let next = match self.cursor {
            Some(i) => i + 1,
            None => 0,
        };
        if next >= self.entries.len() {
            return None;
        }
        self.cursor = Some(next);
        let _ = self.save_cursor();
        Some(&self.entries[next].after)
    }

    /// Get the current entry the cursor is pointing at.
    pub fn current(&self) -> Option<&JournalEntry> {
        self.cursor.map(|i| &self.entries[i])
    }

    /// What would the next undo land on?
    pub fn peek_undo(&self) -> Option<&JournalEntry> {
        self.cursor.map(|i| &self.entries[i])
    }

    /// What would the next redo land on?
    pub fn peek_redo(&self) -> Option<&JournalEntry> {
        let next = match self.cursor {
            Some(i) => i + 1,
            None => 0,
        };
        self.entries.get(next)
    }
    fn journal_path(&self) -> PathBuf {
        self.dir.join("journal.jsonl")
    }

    fn save_cursor(&self) -> Result<()> {
        let path = self.dir.join("cursor");
        let s = self
            .cursor
            .and_then(|i| self.entries.get(i).map(|e| e.id))
            .map(|id| id.to_string())
            .unwrap_or_default();
        fs::write(&path, s).with_context(|| format!("write {}", path.display()))
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn append_line(path: &Path, entry: &JournalEntry) -> Result<()> {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open for append {}", path.display()))?;
    let line = serde_json::to_string(entry)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

fn ensure_excluded(git_dir: &Path) -> Result<()> {
    let info = git_dir.join("info");
    fs::create_dir_all(&info).ok();
    let exclude = info.join("exclude");
    let existing = fs::read_to_string(&exclude).unwrap_or_default();
    if !existing.lines().any(|l| l.trim() == "mergefox/") {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&exclude)?;
        writeln!(f, "mergefox/")?;
    }
    Ok(())
}
