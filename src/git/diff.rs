//! Diff computation for the diff viewer.
//!
//! Produces a `RepoDiff` — a structured representation of a diff that the
//! UI can render without calling back into git internals.
//!
//! Implementation: commit metadata comes from gix; the line-level diff is
//! obtained by running `git show` (which honours local diff drivers, binary
//! detection, and rename heuristics out of the box) and parsing the unified
//! diff output.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use super::ops::{EntryKind, StatusEntry};

/// Max bytes we'll load into memory for *any single blob*.
pub const MAX_BLOB_BYTES: usize = 2 * 1024 * 1024;

/// Max number of diff lines to retain per file.
pub const MAX_LINES_PER_FILE: usize = 5000;

#[derive(Debug, Clone)]
pub struct RepoDiff {
    pub title: String,
    pub commit_message: Option<String>,
    pub commit_author: Option<String>,
    pub files: Box<[FileDiff]>,
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub old_path: Option<PathBuf>,
    pub new_path: Option<PathBuf>,
    pub status: DeltaStatus,
    pub kind: FileKind,
    pub old_size: usize,
    pub new_size: usize,
    pub old_oid: Option<gix::ObjectId>,
    pub new_oid: Option<gix::ObjectId>,
}

impl FileDiff {
    pub fn display_path(&self) -> String {
        self.new_path
            .as_ref()
            .or(self.old_path.as_ref())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<unknown>".to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Typechange,
    Unmodified,
}

impl DeltaStatus {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Modified => "M",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Copied => "C",
            Self::Typechange => "T",
            Self::Unmodified => "·",
        }
    }
}

#[derive(Debug, Clone)]
pub enum FileKind {
    Text {
        hunks: Vec<Hunk>,
        lines_added: usize,
        lines_removed: usize,
        truncated: bool,
    },
    Image {
        ext: String,
    },
    Binary,
    TooLarge,
}

#[derive(Debug, Clone)]
pub struct Hunk {
    pub header: String,
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: LineKind,
    pub content: String,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Remove,
    Meta,
}

/// Build a diff for a single commit (vs its first parent, or empty tree for
/// root commits). Opens its own gix handle so this can be called from a
/// background thread.
///
/// Performance: we make **one** `git show` call that asks for both the
/// raw-diff metadata and the unified patch at the same time. A previous
/// implementation spawned two processes (`git diff-tree --raw` + `git
/// show --patch`), which was ~2× the per-click latency — most visible on
/// macOS where `posix_spawn` plus libgit2's mmap setup dominate.
pub fn diff_for_commit(repo_path: &Path, oid: gix::ObjectId) -> Result<RepoDiff> {
    let profile = std::env::var("MERGEFOX_PROFILE_DIFF").is_ok();
    let t0 = std::time::Instant::now();

    // Commit metadata via gix (cheap, no CLI spawn needed).
    let gix_repo = gix::discover(repo_path).context("open gix repo for diff")?;
    let t_gix_discover = t0.elapsed();

    let (title, commit_message, commit_author) = commit_meta(&gix_repo, oid).unwrap_or_else(|| {
        let s = oid.to_string();
        (s[..7.min(s.len())].to_string(), None, None)
    });
    let t_meta = t0.elapsed();

    // Single `git show` that emits BOTH `--raw` (file OIDs + status) and
    // `--patch` (hunks). `-M` turns on rename detection so the raw lines
    // report `R80` instead of A+D pairs.
    let out = super::cli::run(
        repo_path,
        [
            "show",
            "--no-commit-id",
            "--format=",
            "--raw",
            "--patch",
            "--unified=3",
            "--no-abbrev",
            "-M",
            &oid.to_string(),
        ],
    )
    .context("git show")?;
    let t_show = t0.elapsed();
    let bytes = out.stdout.len();

    let text = out.stdout_str();
    let t_utf8 = t0.elapsed();

    let (raw_entries, patches) = split_raw_and_patch(&text);
    let t_split = t0.elapsed();

    let n_files = raw_entries.len();
    let files = build_file_diffs(raw_entries, patches);
    let t_build = t0.elapsed();

    if profile {
        tracing::debug!(
            target: "mergefox::profile::diff",
            oid = %&oid.to_string()[..7],
            gix_discover_us = t_gix_discover.as_micros() as u64,
            meta_us = (t_meta - t_gix_discover).as_micros() as u64,
            show_us = (t_show - t_meta).as_micros() as u64,
            utf8_us = (t_utf8 - t_show).as_micros() as u64,
            split_us = (t_split - t_utf8).as_micros() as u64,
            build_us = (t_build - t_split).as_micros() as u64,
            total_us = t_build.as_micros() as u64,
            bytes,
            files = n_files,
            "diff profile"
        );
    }

    Ok(RepoDiff {
        title,
        commit_message,
        commit_author,
        files: files.into_boxed_slice(),
    })
}

/// Compute a unified diff text for a single working-tree entry.
///
/// We prefer `git diff HEAD -- <path>` for tracked paths so files that have
/// both staged *and* unstaged edits render as one virtual "working tree
/// commit" against HEAD. If that fails (for example in an unborn repository),
/// we fall back to staged-only / unstaged-only plumbing.
pub fn diff_text_for_working_entry(repo_path: &Path, entry: &StatusEntry) -> Result<String> {
    if !matches!(entry.kind, EntryKind::Untracked) {
        let mut cmd =
            super::cli::GitCommand::new(repo_path).args(["diff", "HEAD", "--binary", "--unified=3", "--"]);
        cmd = cmd.arg(&entry.path);
        if let Ok(text) = run_diff_command(cmd) {
            return Ok(text);
        }
    }

    let text = if matches!(entry.kind, EntryKind::New | EntryKind::Untracked) {
        super::cli::GitCommand::new(repo_path)
            .args(["diff", "--no-index", "--binary", "--unified=3", "--", "/dev/null"])
            .arg(&entry.path)
    } else if entry.kind == EntryKind::Deleted {
        super::cli::GitCommand::new(repo_path)
            .args(["diff", "--no-index", "--binary", "--unified=3", "--"])
            .arg(&entry.path)
            .arg("/dev/null")
    } else if entry.staged {
        super::cli::GitCommand::new(repo_path)
            .args(["diff", "--cached", "--binary", "--unified=3", "--"])
            .arg(&entry.path)
    } else {
        super::cli::GitCommand::new(repo_path)
            .args(["diff", "--binary", "--unified=3", "--"])
            .arg(&entry.path)
    };

    run_diff_command(text)
}

/// Convert cached unified diff text for a working-tree entry into the same
/// `FileDiff` structure used by commit diffs, so the UI can reuse the normal
/// renderer without a special-case text widget.
pub fn file_diff_for_working_entry(entry: &StatusEntry, patch_text: &str) -> FileDiff {
    let fallback_status = delta_status_for_working_entry(entry);
    let (fallback_old_path, fallback_new_path) = fallback_paths_for_working_entry(entry);

    let patch = parse_patches(patch_text)
        .into_iter()
        .find(|p| {
            p.new_path.as_ref() == Some(&entry.path) || p.old_path.as_ref() == Some(&entry.path)
        });

    let old_path = patch
        .as_ref()
        .and_then(|p| p.old_path.clone())
        .or_else(|| fallback_old_path.clone());
    let new_path = patch
        .as_ref()
        .and_then(|p| p.new_path.clone())
        .or_else(|| fallback_new_path.clone());

    let ext = extension_of(new_path.as_ref().or(old_path.as_ref()));
    let is_image = ext.as_deref().map(is_image_ext).unwrap_or(false);
    let is_binary = patch.as_ref().map(|p| p.is_binary).unwrap_or(false);

    let kind = if is_image {
        FileKind::Image {
            ext: ext.unwrap_or_else(|| "img".into()),
        }
    } else if is_binary {
        FileKind::Binary
    } else {
        let (hunks, lines_added, lines_removed, truncated) = if let Some(patch) = patch.as_ref() {
            let mut add = 0usize;
            let mut rem = 0usize;
            let mut total = 0usize;
            for hunk in &patch.hunks {
                for line in &hunk.lines {
                    match line.kind {
                        LineKind::Add => add += 1,
                        LineKind::Remove => rem += 1,
                        _ => {}
                    }
                    total += 1;
                }
            }
            (patch.hunks.clone(), add, rem, total >= MAX_LINES_PER_FILE)
        } else {
            (Vec::new(), 0, 0, false)
        };

        FileKind::Text {
            hunks,
            lines_added,
            lines_removed,
            truncated,
        }
    };

    FileDiff {
        old_path,
        new_path,
        status: fallback_status,
        kind,
        old_size: 0,
        new_size: 0,
        old_oid: None,
        new_oid: None,
    }
}

// ---- internal helpers ----

fn commit_meta(
    gix_repo: &gix::Repository,
    oid: gix::ObjectId,
) -> Option<(String, Option<String>, Option<String>)> {
    let commit = gix_repo.find_object(oid).ok()?.try_into_commit().ok()?;
    let parent_ids: Vec<gix::ObjectId> = commit.parent_ids().map(|id| id.detach()).collect();
    let msg = commit.message().ok()?;
    let summary = msg.summary().to_string();
    let author_name = commit.author().ok().map(|a| a.name.to_string());

    let oid_s = oid.to_string();
    let short = &oid_s[..7.min(oid_s.len())];
    let title = if let Some(pid) = parent_ids.first() {
        let ps = pid.to_string();
        format!("{} → {short}", &ps[..7.min(ps.len())])
    } else {
        format!("<root> → {short}")
    };

    Some((title, Some(summary), author_name))
}

fn run_diff_command(cmd: super::cli::GitCommand) -> Result<String> {
    let output = cmd.run_raw().context("run git diff")?;
    let code = output.status.code().unwrap_or(-1);
    if code != 0 && code != 1 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn delta_status_for_working_entry(entry: &StatusEntry) -> DeltaStatus {
    match entry.kind {
        EntryKind::New | EntryKind::Untracked => DeltaStatus::Added,
        EntryKind::Modified | EntryKind::Conflicted => DeltaStatus::Modified,
        EntryKind::Deleted => DeltaStatus::Deleted,
        EntryKind::Renamed => DeltaStatus::Renamed,
        EntryKind::Typechange => DeltaStatus::Typechange,
    }
}

fn fallback_paths_for_working_entry(entry: &StatusEntry) -> (Option<PathBuf>, Option<PathBuf>) {
    match delta_status_for_working_entry(entry) {
        DeltaStatus::Added => (None, Some(entry.path.clone())),
        DeltaStatus::Deleted => (Some(entry.path.clone()), None),
        _ => (Some(entry.path.clone()), Some(entry.path.clone())),
    }
}

/// A file entry from `git diff-tree --raw`.
struct RawEntry {
    old_oid: Option<gix::ObjectId>,
    new_oid: Option<gix::ObjectId>,
    status: char,
    /// For renamed/copied: new path is stored here; old path in `orig_path`.
    path: PathBuf,
    orig_path: Option<PathBuf>,
}

/// Split the combined `git show --raw --patch` output into the raw
/// section (metadata lines beginning with `:`) and the patch section
/// (everything from the first `diff --git` header onward).
fn split_raw_and_patch(text: &str) -> (Vec<RawEntry>, Vec<PatchSection>) {
    let mut raw_lines: Vec<&str> = Vec::new();
    let mut patch_lines: Vec<&str> = Vec::new();
    let mut in_patch = false;
    for line in text.lines() {
        if !in_patch {
            if line.starts_with("diff --git ") {
                in_patch = true;
                patch_lines.push(line);
            } else if line.starts_with(':') {
                raw_lines.push(line);
            }
            // Blank lines between raw and patch are ignored.
        } else {
            patch_lines.push(line);
        }
    }
    let patch_text = patch_lines.join("\n");
    (parse_raw_lines(&raw_lines), parse_patches(&patch_text))
}

/// Parse `git show --raw` (no `-z`) lines.
///
/// Format per line:
///   `:<old_mode> <new_mode> <old_sha> <new_sha> <status>[<score>]\t<path>`
/// For renames / copies:
///   `:<…> R<score>\t<old_path>\t<new_path>`
fn parse_raw_lines(lines: &[&str]) -> Vec<RawEntry> {
    let mut entries = Vec::with_capacity(lines.len());
    for line in lines {
        let rest = match line.strip_prefix(':') {
            Some(r) => r,
            None => continue,
        };
        let mut tab_iter = rest.split('\t');
        let Some(header) = tab_iter.next() else {
            continue;
        };
        let fields: Vec<&str> = header.split(' ').collect();
        if fields.len() < 5 {
            continue;
        }
        let old_sha = fields[2];
        let new_sha = fields[3];
        let status_str = fields[4];
        let status = status_str.chars().next().unwrap_or('M');

        let (path, orig_path) = if matches!(status, 'R' | 'C') {
            let old = tab_iter.next().unwrap_or("");
            let new = tab_iter.next().unwrap_or("");
            (PathBuf::from(new), Some(PathBuf::from(old)))
        } else {
            let p = tab_iter.next().unwrap_or("");
            (PathBuf::from(p), None)
        };

        entries.push(RawEntry {
            old_oid: non_zero_oid_hex(old_sha),
            new_oid: non_zero_oid_hex(new_sha),
            status,
            path,
            orig_path,
        });
    }
    entries
}

fn non_zero_oid_hex(hex: &str) -> Option<gix::ObjectId> {
    if hex.is_empty() || hex.chars().all(|c| c == '0') {
        return None;
    }
    gix::ObjectId::from_hex(hex.as_bytes()).ok()
}

/// A parsed patch section from unified diff output.
struct PatchSection {
    old_path: Option<PathBuf>,
    new_path: Option<PathBuf>,
    hunks: Vec<Hunk>,
    is_binary: bool,
}

/// Parse unified diff output from `git show --patch` into per-file sections.
fn parse_patches(text: &str) -> Vec<PatchSection> {
    let mut patches: Vec<PatchSection> = Vec::new();
    let mut current: Option<PatchSection> = None;
    let mut current_hunk: Option<Hunk> = None;
    let mut old_lineno = 0u32;
    let mut new_lineno = 0u32;
    let mut total_lines = 0usize;

    for raw_line in text.lines() {
        if raw_line.starts_with("diff --git ") {
            // Flush any in-progress patch.
            flush_patch(&mut current, &mut current_hunk, &mut patches);
            current = Some(PatchSection {
                old_path: None,
                new_path: None,
                hunks: Vec::new(),
                is_binary: false,
            });
            current_hunk = None;
            old_lineno = 0;
            new_lineno = 0;
            total_lines = 0;
        } else if raw_line.starts_with("--- ") {
            if let Some(ref mut p) = current {
                let path_part = raw_line.strip_prefix("--- ").unwrap_or("");
                // `a/path` or `/dev/null`
                let path_part = path_part.strip_prefix("a/").unwrap_or(path_part);
                if path_part != "/dev/null" {
                    p.old_path = Some(PathBuf::from(path_part));
                }
            }
        } else if raw_line.starts_with("+++ ") {
            if let Some(ref mut p) = current {
                let path_part = raw_line.strip_prefix("+++ ").unwrap_or("");
                let path_part = path_part.strip_prefix("b/").unwrap_or(path_part);
                if path_part != "/dev/null" {
                    p.new_path = Some(PathBuf::from(path_part));
                }
            }
        } else if raw_line.starts_with("@@ ") {
            if let Some(ref mut p) = current {
                if let Some(h) = current_hunk.take() {
                    p.hunks.push(h);
                }
                let (os, ol, ns, nl) = parse_hunk_header(raw_line);
                old_lineno = os;
                new_lineno = ns;
                current_hunk = Some(Hunk {
                    header: raw_line.to_owned(),
                    old_start: os,
                    old_lines: ol,
                    new_start: ns,
                    new_lines: nl,
                    lines: Vec::new(),
                });
            }
        } else if raw_line.starts_with("Binary files ") {
            if let Some(ref mut p) = current {
                p.is_binary = true;
            }
        } else if let Some(ref mut h) = current_hunk {
            if total_lines >= MAX_LINES_PER_FILE {
                // Mark the containing file as truncated when we flush.
                continue;
            }
            if let Some(rest) = raw_line.strip_prefix('+') {
                h.lines.push(DiffLine {
                    kind: LineKind::Add,
                    content: rest.to_owned(),
                    old_lineno: None,
                    new_lineno: Some(new_lineno),
                });
                new_lineno += 1;
                total_lines += 1;
            } else if let Some(rest) = raw_line.strip_prefix('-') {
                h.lines.push(DiffLine {
                    kind: LineKind::Remove,
                    content: rest.to_owned(),
                    old_lineno: Some(old_lineno),
                    new_lineno: None,
                });
                old_lineno += 1;
                total_lines += 1;
            } else if let Some(rest) = raw_line.strip_prefix(' ') {
                h.lines.push(DiffLine {
                    kind: LineKind::Context,
                    content: rest.to_owned(),
                    old_lineno: Some(old_lineno),
                    new_lineno: Some(new_lineno),
                });
                old_lineno += 1;
                new_lineno += 1;
            } else if raw_line.starts_with('\\') {
                h.lines.push(DiffLine {
                    kind: LineKind::Meta,
                    content: raw_line.to_owned(),
                    old_lineno: None,
                    new_lineno: None,
                });
            }
        }
    }
    flush_patch(&mut current, &mut current_hunk, &mut patches);
    patches
}

fn flush_patch(
    current: &mut Option<PatchSection>,
    current_hunk: &mut Option<Hunk>,
    patches: &mut Vec<PatchSection>,
) {
    if let Some(mut p) = current.take() {
        if let Some(h) = current_hunk.take() {
            p.hunks.push(h);
        }
        patches.push(p);
    }
}

fn parse_hunk_header(line: &str) -> (u32, u32, u32, u32) {
    // `@@ -old_start[,old_lines] +new_start[,new_lines] @@`
    let inner = line
        .strip_prefix("@@ ")
        .and_then(|s| s.find(" @@").map(|i| &s[..i]))
        .unwrap_or("");
    let mut old_start = 1u32;
    let mut old_lines = 1u32;
    let mut new_start = 1u32;
    let mut new_lines = 1u32;
    for part in inner.split(' ') {
        if let Some(s) = part.strip_prefix('-') {
            let v: Vec<&str> = s.splitn(2, ',').collect();
            old_start = v.first().and_then(|n| n.parse().ok()).unwrap_or(1);
            old_lines = v.get(1).and_then(|n| n.parse().ok()).unwrap_or(1);
        } else if let Some(s) = part.strip_prefix('+') {
            let v: Vec<&str> = s.splitn(2, ',').collect();
            new_start = v.first().and_then(|n| n.parse().ok()).unwrap_or(1);
            new_lines = v.get(1).and_then(|n| n.parse().ok()).unwrap_or(1);
        }
    }
    (old_start, old_lines, new_start, new_lines)
}

/// Combine raw entries (paths, OIDs, status) with parsed patches (hunks, lines)
/// into the final `Vec<FileDiff>`.
fn build_file_diffs(raw: Vec<RawEntry>, patches: Vec<PatchSection>) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::with_capacity(raw.len());

    for entry in raw {
        let (old_path, new_path) = match entry.status {
            'A' => (None, Some(entry.path)),
            'D' => (Some(entry.path), None),
            'R' | 'C' => (entry.orig_path, Some(entry.path)),
            _ => {
                let p = entry.path.clone();
                (Some(p.clone()), Some(p))
            }
        };

        let status = match entry.status {
            'A' => DeltaStatus::Added,
            'D' => DeltaStatus::Deleted,
            'R' => DeltaStatus::Renamed,
            'C' => DeltaStatus::Copied,
            'T' => DeltaStatus::Typechange,
            _ => DeltaStatus::Modified,
        };

        let display = new_path.as_ref().or(old_path.as_ref());
        let ext = extension_of(display);
        let is_image = ext.as_deref().map(is_image_ext).unwrap_or(false);

        // Find corresponding patch section by matching paths. We require
        // at least one CONCRETE (non-None) path to agree — otherwise
        // `None == None` makes every "Added" file match the first patch
        // section, which is why the initial commit was showing .gitignore
        // content for every file.
        let patch = patches.iter().find(|p| {
            let pnew = p.new_path.as_ref();
            let pold = p.old_path.as_ref();
            (pnew.is_some() && pnew == new_path.as_ref())
                || (pold.is_some() && pold == old_path.as_ref())
                || (pnew.is_some() && pnew == old_path.as_ref())
        });

        let is_binary = patch.map(|p| p.is_binary).unwrap_or(false);

        let kind = if is_image {
            FileKind::Image {
                ext: ext.unwrap_or_else(|| "img".into()),
            }
        } else if is_binary {
            FileKind::Binary
        } else {
            let (hunks, lines_added, lines_removed, truncated) = if let Some(ps) = patch {
                let mut add = 0usize;
                let mut rem = 0usize;
                let mut total = 0usize;
                for h in &ps.hunks {
                    for l in &h.lines {
                        match l.kind {
                            LineKind::Add => add += 1,
                            LineKind::Remove => rem += 1,
                            _ => {}
                        }
                        total += 1;
                    }
                }
                let trunc = total >= MAX_LINES_PER_FILE;
                (ps.hunks.clone(), add, rem, trunc)
            } else {
                (Vec::new(), 0, 0, false)
            };
            FileKind::Text {
                hunks,
                lines_added,
                lines_removed,
                truncated,
            }
        };

        files.push(FileDiff {
            old_path,
            new_path,
            status,
            kind,
            old_size: 0, // not provided by diff-tree --raw without extra lookup
            new_size: 0,
            old_oid: entry.old_oid,
            new_oid: entry.new_oid,
        });
    }

    files
}

// ---- blob loading ----

/// Load a blob's raw bytes from the gix object store.
pub fn load_blob_bytes(
    gix_repo: &gix::Repository,
    oid: Option<gix::ObjectId>,
) -> Option<Arc<[u8]>> {
    let oid = oid?;
    let obj = gix_repo.find_object(oid).ok()?;
    let blob = obj.try_into_blob().ok()?;
    let content = blob.data.as_slice();
    if content.len() > MAX_BLOB_BYTES {
        return None;
    }
    Some(Arc::from(content))
}

/// Load a blob's content as a UTF-8 string.
pub fn load_blob_text(gix_repo: &gix::Repository, oid: Option<gix::ObjectId>) -> Option<String> {
    let bytes = load_blob_bytes(gix_repo, oid)?;
    std::str::from_utf8(&bytes).ok().map(str::to_owned)
}

// ---- utilities ----

fn extension_of(path: Option<&PathBuf>) -> Option<String> {
    path.and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
}

fn is_image_ext(ext: &str) -> bool {
    matches!(
        ext,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tiff" | "tif"
    )
}
