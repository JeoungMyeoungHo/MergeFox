//! Synchronous, local-only git operations via the system `git` binary.
//!
//! All functions accept the repository working-directory path; they delegate
//! every operation to `git` so hooks, signing, credential helpers, and local
//! config are always respected — same as running the command in a terminal.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct StatusEntry {
    pub path: PathBuf,
    pub kind: EntryKind,
    pub staged: bool,
    pub unstaged: bool,
    pub conflicted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    New,
    Modified,
    Deleted,
    Renamed,
    Typechange,
    Untracked,
    Conflicted,
}

impl EntryKind {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::New => "A",
            Self::Modified => "M",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Typechange => "T",
            Self::Untracked => "?",
            Self::Conflicted => "!",
        }
    }
}

/// Summarize the working tree / index into a flat list suitable for the
/// commit dialog. Entries where both staged and unstaged are true mean
/// the file has staged changes AND further unstaged tweaks on top.
pub fn status_entries(repo_path: &Path) -> Result<Vec<StatusEntry>> {
    let out = super::cli::run(
        repo_path,
        ["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    Ok(parse_status_z(&out.stdout))
}

/// Parse NUL-terminated `git status --porcelain=v1 -z` output.
///
/// Each entry in the stream: `XY<sp><path>\0` where X=index status,
/// Y=working-tree status. For renames / copies there is a second
/// NUL-terminated token immediately after (the original path).
fn parse_status_z(data: &[u8]) -> Vec<StatusEntry> {
    let mut entries = Vec::new();
    let mut pos = 0;

    while pos < data.len() {
        // Need at least "XY " (3 bytes) plus something.
        if pos + 3 > data.len() {
            break;
        }
        let x = data[pos] as char;
        let y = data[pos + 1] as char;
        // data[pos + 2] is a space.
        pos += 3;

        // Read path until NUL.
        let path_start = pos;
        while pos < data.len() && data[pos] != 0 {
            pos += 1;
        }
        if pos >= data.len() {
            break; // truncated entry — ignore
        }
        let path = PathBuf::from(String::from_utf8_lossy(&data[path_start..pos]).as_ref());
        pos += 1; // consume NUL

        // Renames / copies have an extra NUL-terminated original path.
        if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
            while pos < data.len() && data[pos] != 0 {
                pos += 1;
            }
            if pos < data.len() {
                pos += 1;
            }
        }

        let is_ignored = x == '!' && y == '!';
        if is_ignored {
            continue;
        }

        // Unmerged / conflict combinations.
        let conflicted = matches!(
            (x, y),
            ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D') | ('D', 'U') | ('U', 'D')
        );
        let staged = !conflicted && matches!(x, 'M' | 'A' | 'D' | 'R' | 'C' | 'T');
        let unstaged = !conflicted && matches!(y, 'M' | 'D' | 'R' | 'T');
        let is_untracked = x == '?' && y == '?';

        let kind = if conflicted {
            EntryKind::Conflicted
        } else if is_untracked {
            EntryKind::Untracked
        } else if x == 'A' {
            EntryKind::New
        } else if x == 'D' || y == 'D' {
            EntryKind::Deleted
        } else if x == 'R' || y == 'R' {
            EntryKind::Renamed
        } else if x == 'T' || y == 'T' {
            EntryKind::Typechange
        } else {
            EntryKind::Modified
        };

        entries.push(StatusEntry {
            path,
            kind,
            staged,
            unstaged,
            conflicted,
        });
    }

    // Sort: conflicted first, then staged, then untracked.
    entries.sort_by_key(|e| {
        (
            !e.conflicted,
            !e.staged,
            matches!(e.kind, EntryKind::Untracked),
            e.path.clone(),
        )
    });
    entries
}

/// `git add -A` — stage every change including deletions and new files.
/// Returns approximate staged-file count (from `git diff --cached`).
pub fn stage_all(repo_path: &Path) -> Result<usize> {
    super::cli::run(repo_path, ["add", "-A"]).context("git add -A")?;
    let out = super::cli::run(repo_path, ["diff", "--cached", "--name-only"])
        .unwrap_or_else(|_| super::cli::CliOutput {
            stdout: vec![],
            stderr: vec![],
            status: 0,
        });
    Ok(out.stdout_str().lines().count())
}

/// Stage a specific set of paths (relative to the workdir).
/// Uses `-A` so deletions are also staged.
pub fn stage_paths(repo_path: &Path, paths: &[&Path]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut cmd = super::cli::GitCommand::new(repo_path).args(["add", "-A", "--"]);
    for p in paths {
        cmd = cmd.arg(p);
    }
    cmd.run().context("git add paths")?;
    Ok(())
}

/// Unstage a specific set of paths (move them back to working-tree-only).
/// Uses `git reset HEAD -- <paths>` which clears the index entries without
/// touching the working tree. For paths that were newly-added (no HEAD
/// entry), `git reset` falls back to `git rm --cached`.
pub fn unstage_paths(repo_path: &Path, paths: &[&Path]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut cmd = super::cli::GitCommand::new(repo_path).args(["reset", "HEAD", "--"]);
    for p in paths {
        cmd = cmd.arg(p);
    }
    // `git reset HEAD -- <newly-added>` exits 1 but still unstages. We
    // run_raw and accept both 0 and 1.
    let output = cmd.run_raw().context("git reset HEAD")?;
    if !output.status.success() {
        // Exit 1 is common for mixed results; inspect stderr for real errors.
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty()
            && !stderr.contains("Unstaged changes after reset")
            && !stderr.contains("warning")
        {
            // It's a real failure only when stderr has something other than
            // the routine "Unstaged changes after reset" diagnostic.
            anyhow::bail!("git reset failed: {}", stderr.trim());
        }
    }
    Ok(())
}

/// Create a commit on HEAD from the current index. Returns the new OID.
pub fn commit(repo_path: &Path, message: &str) -> Result<gix::ObjectId> {
    super::cli::GitCommand::new(repo_path)
        .args(["commit", "-F", "-"])
        .stdin(message.as_bytes().to_vec())
        .run()
        .context("git commit")?;
    head_oid(repo_path)
}

/// Amend HEAD with an optional new message. Returns the amended OID.
pub fn amend(repo_path: &Path, new_message: Option<&str>) -> Result<gix::ObjectId> {
    let cmd = match new_message {
        Some(msg) => super::cli::GitCommand::new(repo_path)
            .args(["commit", "--amend", "-F", "-"])
            .stdin(msg.as_bytes().to_vec()),
        None => super::cli::GitCommand::new(repo_path).args(["commit", "--amend", "--no-edit"]),
    };
    cmd.run().context("git commit --amend")?;
    head_oid(repo_path)
}

fn head_oid(repo_path: &Path) -> Result<gix::ObjectId> {
    let s = super::cli::run_line(repo_path, ["rev-parse", "HEAD"])?;
    gix::ObjectId::from_hex(s.trim().as_bytes()).context("parse HEAD OID")
}

// ---- stash ----

/// Create a stash entry including untracked files. Returns the stash OID.
pub fn stash_push(repo_path: &Path, message: &str) -> Result<gix::ObjectId> {
    super::cli::run(repo_path, ["stash", "push", "-u", "-m", message])
        .context("git stash push")?;
    let s = super::cli::run_line(repo_path, ["rev-parse", "stash@{0}"])
        .context("rev-parse stash@{0}")?;
    gix::ObjectId::from_hex(s.trim().as_bytes()).context("parse stash OID")
}

/// Pop (apply + drop) a stash by its 0-based index.
pub fn stash_pop(repo_path: &Path, index: usize) -> Result<()> {
    let refspec = format!("stash@{{{index}}}");
    super::cli::run(repo_path, ["stash", "pop", refspec.as_str()])
        .context("git stash pop")?;
    Ok(())
}

/// List all stash entries. Cheapest full implementation: one `git stash list`
/// call with a machine-readable format.
pub fn stash_list(repo_path: &Path) -> Result<Vec<StashEntry>> {
    let sep = '\x1f'; // ASCII unit-separator, safe in git format strings
    let fmt = format!("%gd{sep}%H{sep}%gs");
    let out = super::cli::run(
        repo_path,
        ["stash", "list", &format!("--format={fmt}")],
    )?;
    let stdout = out.stdout_str();
    let mut entries = Vec::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(3, sep).collect();
        if parts.len() < 3 {
            continue;
        }
        let index = parts[0]
            .strip_prefix("stash@{")
            .and_then(|s| s.strip_suffix('}'))
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let oid = gix::ObjectId::from_hex(parts[1].trim().as_bytes())
            .unwrap_or_else(|_| gix::ObjectId::null(gix::hash::Kind::Sha1));
        let message = parts[2].to_owned();
        entries.push(StashEntry {
            index,
            message,
            oid,
        });
    }
    Ok(entries)
}

#[derive(Debug, Clone)]
pub struct StashEntry {
    pub index: usize,
    pub message: String,
    pub oid: gix::ObjectId,
}
