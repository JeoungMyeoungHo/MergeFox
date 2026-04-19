//! Split a single commit into N smaller commits, each carrying a subset
//! of the original commit's changes.
//!
//! Motivation
//! ----------
//! A common workflow mistake: the user stages a messy afternoon of
//! editing as one commit ("feat: add widget + fix typo + refactor
//! helper") and later wants to break it apart so reviewers can reason
//! about each piece independently. A raw `git` session for this is
//! fiddly — detach, `reset --soft`, replay piecewise `git add -p` runs,
//! commit each, rebase descendants. Every step is a place to fat-finger
//! a destructive mistake. This module centralises the dance behind a
//! typed API with a single safety envelope:
//!
//!   * auto-stash dirty working tree (refuses on very large diffs),
//!   * backup tag `mergefox/split/<ISO-UTC-ts>` at the old HEAD,
//!   * rollback-on-any-error that restores backup → pops stash.
//!
//! The v1 granularity restriction
//! -------------------------------
//! `discover_hunks` enumerates every unified-diff hunk in the target
//! commit so the UI can list them for picking. However, `split_commit`
//! enforces that **all hunks of a given file belong to the same
//! `SplitPart`**. That is: hunks do not get split across parts within a
//! single file — whole files do.
//!
//! WHY this restriction: sub-file hunk-level splitting requires
//! generating a fresh unified diff per part whose line offsets re-anchor
//! against the state left by prior parts. Getting the line-offset
//! accounting right across binary files, mode changes, rename
//! detection, and `\ No newline at end of file` markers is a
//! non-trivial patch-editing library in its own right. The whole-file
//! grouping covers the overwhelmingly common case ("split this commit
//! into feat/fix/refactor pieces where each piece touched different
//! files") with a correctness profile we can unit-test end-to-end.
//!
//! Cross-hunk-within-a-file splitting is recorded as a v2 follow-up in
//! the doc-comment above `split_commit`. The UI surfaces a clear error
//! if the user assigns two hunks of the same file to different parts.
//!
//! Approach
//! --------
//! 1. Validate the plan: ≥2 parts, every original hunk assigned to
//!    exactly one part, no file straddling parts, non-empty messages.
//! 2. Auto-stash dirty tracked files.
//! 3. Create a backup tag at HEAD.
//! 4. Detach HEAD at the target's parent.
//! 5. For each part in order:
//!      * `git checkout <target> -- <files-for-part>` to overlay only
//!        that part's files onto the index + working tree.
//!      * Commit with the original author identity preserved via
//!        `GIT_AUTHOR_*` env vars (committer becomes "now").
//! 6. Rebase descendants of the target (the "keep" list between
//!    `target..original-head`) onto the new tip.
//! 7. Move the branch ref to the new tip. Pop the stash.
//!
//! Any error after step 3 triggers rollback: cherry-pick / rebase abort,
//! `checkout -f <original-branch>` (or the backup tag if detached),
//! `reset --hard <backup-tag>`, `stash pop`.
//!
//! Merge commits and root commits are rejected up front — splitting a
//! merge has no obvious "parent" to rebuild onto and the UI never offers
//! split on either.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::repo::{auto_stash_path, AutoStashOpts, AutoStashOutcome};

/// A reference to a single hunk inside the target commit's diff.
/// `file` is the post-commit path (new side of the diff). `hunk_index`
/// is 0-based in the order `git show` emitted them for that file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkRef {
    pub file: PathBuf,
    pub hunk_index: usize,
}

/// One new commit the split will produce.
///
/// `message` is the commit message. `hunks` are the original-commit
/// hunks that should land in this commit. Order of `parts` in the
/// enclosing `SplitPlan` determines apply order — `parts[0]` becomes
/// the oldest of the new commits.
#[derive(Debug, Clone)]
pub struct SplitPart {
    pub message: String,
    pub hunks: Vec<HunkRef>,
}

/// A full plan for splitting `target_oid` into `parts.len()` commits.
#[derive(Debug, Clone)]
pub struct SplitPlan {
    pub target_oid: gix::ObjectId,
    pub parts: Vec<SplitPart>,
}

/// Outcome of `split_commit`, from the UI thread's point of view.
#[derive(Debug, Clone)]
pub enum SplitOutcome {
    /// The split ran cleanly. `new_commit_oids` is oldest-first (same
    /// order as `plan.parts`); `new_head_oid` is the tip of the
    /// rewritten branch after any descendants were replayed.
    Success {
        new_head_oid: gix::ObjectId,
        new_commit_oids: Vec<gix::ObjectId>,
        backup_tag: String,
    },
    /// Something went wrong; we rolled back to the pre-split state. If
    /// `backup_tag_created` is `Some`, the tag is still on disk so the
    /// user has an explicit undo anchor.
    Aborted {
        reason: String,
        backup_tag_created: Option<String>,
    },
}

/// One unified-diff hunk in the target commit, as surfaced to the UI.
/// `preview` is the first few body lines (context + changes) so the UI
/// can render a compact summary without reparsing the raw diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredHunk {
    pub file: PathBuf,
    pub hunk_index: usize,
    pub header: String,
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub preview: String,
}

/// Backup-tag namespace helper. Other history-rewriting modules use the
/// same shape (`mergefox/<namespace>/<ISO-UTC-ts>`) so `git tag -l
/// 'mergefox/*'` finds them all for retention / undo UIs. Returns the
/// namespace string to be fed into `build_backup_tag_name`.
pub fn backup_tag_name_for(namespace: &str) -> &str {
    // Kept as a helper even though it's a passthrough: callers that
    // need the full timestamped name call `build_backup_tag_name` with
    // the same namespace, and both sides sharing this function ensures
    // the spelling stays in sync if we ever rename the prefix.
    namespace
}

/// Construct the full `mergefox/<namespace>/<YYYYMMDDTHHMMSSZ>` tag
/// name. Split out so tests can lock the format without reaching for
/// wallclock time.
pub fn build_backup_tag_name(namespace: &str, unix_seconds: u64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix_seconds(unix_seconds);
    format!(
        "mergefox/{}/{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        namespace, y, mo, d, h, mi, s
    )
}

// Pure arithmetic Gregorian civil-date conversion; mirrors the one in
// `basket_ops` (see its block comment for the reference algorithm).
// Duplicated here rather than depending on `basket_ops` because the
// two modules are peers and neither should own the other's internals.
fn civil_from_unix_seconds(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = (y + if m <= 2 { 1 } else { 0 }) as i32;

    (year, m, d, hour, minute, second)
}

fn now_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ============================================================
// discover_hunks
// ============================================================

/// Enumerate every unified-diff hunk in the target commit so the UI
/// can list them in the picker.
///
/// Implementation notes: we call `git show --format= --unified=3
/// <oid>` and parse the resulting unified diff by hand. We deliberately
/// do NOT reuse `diff::diff_for_commit` because that path normalises
/// binary / rename / mode-change entries in ways that strip the raw
/// `@@` header text — which is exactly what the UI wants to show.
pub fn discover_hunks(repo_path: &Path, target: gix::ObjectId) -> Result<Vec<DiscoveredHunk>> {
    let output = super::cli::run(
        repo_path,
        [
            "show",
            "--format=",
            "--no-color",
            "--unified=3",
            "--no-prefix",
            &target.to_string(),
        ],
    )
    .with_context(|| format!("git show {}", target))?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    Ok(parse_unified_diff_hunks(&text))
}

/// Parse unified-diff text produced by `git show --no-prefix` into a
/// flat list of hunks tagged with their owning file and ordinal.
///
/// WHY `--no-prefix`: with the default `a/`/`b/` prefixes we'd have to
/// strip them anyway and handle the "same path on both sides" common
/// case. With no prefixes, the `diff --git foo.rs foo.rs` line yields
/// the path straight up.
fn parse_unified_diff_hunks(text: &str) -> Vec<DiscoveredHunk> {
    let mut out: Vec<DiscoveredHunk> = Vec::new();
    let mut current_file: Option<PathBuf> = None;
    let mut file_hunk_counter: usize = 0;
    // Buffer for the body of the current hunk so we can compute a
    // preview. We don't need every line, but first ~6 or so give the
    // UI enough context for a meaningful tooltip.
    let mut current_hunk: Option<PendingHunk> = None;

    for raw_line in text.split_inclusive('\n') {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);

        if let Some(rest) = line.strip_prefix("diff --git ") {
            // Flush whatever hunk was in flight before transitioning
            // to the new file.
            if let Some(h) = current_hunk.take() {
                out.push(h.into_discovered());
            }
            current_file = parse_diff_git_paths(rest);
            file_hunk_counter = 0;
            continue;
        }

        // Chaff lines between `diff --git` and the first `@@` — mode
        // changes, `index ...`, `---`, `+++`. Skip them; they don't
        // contribute to hunks.
        if line.starts_with("---")
            || line.starts_with("+++")
            || line.starts_with("index ")
            || line.starts_with("new file mode")
            || line.starts_with("deleted file mode")
            || line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("rename from")
            || line.starts_with("rename to")
            || line.starts_with("copy from")
            || line.starts_with("copy to")
            || line.starts_with("similarity index")
            || line.starts_with("dissimilarity index")
            || line.starts_with("Binary files")
            || line.starts_with("GIT binary patch")
        {
            if let Some(h) = current_hunk.take() {
                out.push(h.into_discovered());
            }
            continue;
        }

        if let Some(hdr) = parse_hunk_header(line) {
            if let Some(h) = current_hunk.take() {
                out.push(h.into_discovered());
            }
            if let Some(file) = current_file.as_ref() {
                let idx = file_hunk_counter;
                file_hunk_counter += 1;
                current_hunk = Some(PendingHunk {
                    file: file.clone(),
                    hunk_index: idx,
                    header: line.to_string(),
                    old_start: hdr.old_start,
                    old_count: hdr.old_count,
                    new_start: hdr.new_start,
                    new_count: hdr.new_count,
                    preview_lines: Vec::new(),
                });
            }
            continue;
        }

        if let Some(h) = current_hunk.as_mut() {
            // Body lines: context / additions / deletions / "\ No
            // newline at end of file". Keep the first handful for the
            // preview.
            if h.preview_lines.len() < 6 {
                h.preview_lines.push(line.to_string());
            }
        }
    }

    if let Some(h) = current_hunk.take() {
        out.push(h.into_discovered());
    }
    out
}

struct PendingHunk {
    file: PathBuf,
    hunk_index: usize,
    header: String,
    old_start: u32,
    old_count: u32,
    new_start: u32,
    new_count: u32,
    preview_lines: Vec<String>,
}

impl PendingHunk {
    fn into_discovered(self) -> DiscoveredHunk {
        DiscoveredHunk {
            file: self.file,
            hunk_index: self.hunk_index,
            header: self.header,
            old_start: self.old_start,
            old_count: self.old_count,
            new_start: self.new_start,
            new_count: self.new_count,
            preview: self.preview_lines.join("\n"),
        }
    }
}

struct HunkHeader {
    old_start: u32,
    old_count: u32,
    new_start: u32,
    new_count: u32,
}

/// Parse `@@ -old_start[,old_count] +new_start[,new_count] @@ [hint]`.
/// Returns `None` for any non-`@@` line.
fn parse_hunk_header(line: &str) -> Option<HunkHeader> {
    let rest = line.strip_prefix("@@ ")?;
    let end = rest.find(" @@")?;
    let range = &rest[..end];
    let mut parts = range.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let (old_start, old_count) = parse_range(old)?;
    let (new_start, new_count) = parse_range(new)?;
    Some(HunkHeader {
        old_start,
        old_count,
        new_start,
        new_count,
    })
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    match s.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// Extract the new-side path from a `diff --git a b` header line.
///
/// With `--no-prefix` both sides are plain paths. When they differ
/// (rename) we pick the new side because that's what the rest of the
/// tree stores. Quoted paths (core.quotepath default) are unquoted.
fn parse_diff_git_paths(rest: &str) -> Option<PathBuf> {
    let tokens = tokenise_diff_git(rest)?;
    if tokens.len() < 2 {
        return None;
    }
    Some(PathBuf::from(tokens.last().unwrap().clone()))
}

/// Split a `diff --git` payload into its two path tokens. Paths may be
/// wrapped in double quotes when they contain funny bytes; we honour
/// the opening `"` and consume until the matching close. Everything
/// else is whitespace-delimited.
fn tokenise_diff_git(rest: &str) -> Option<Vec<String>> {
    let bytes = rest.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'"' {
            i += 1;
            let mut buf = String::new();
            while i < bytes.len() && bytes[i] != b'"' {
                // `\"` / `\\` — minimal dequoting; full octal escapes
                // land as-is (rare, and the picker still shows the
                // correct file name modulo weird bytes).
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    buf.push(bytes[i + 1] as char);
                    i += 2;
                    continue;
                }
                buf.push(bytes[i] as char);
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            out.push(buf);
        } else {
            let start = i;
            while i < bytes.len() && bytes[i] != b' ' {
                i += 1;
            }
            out.push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

// ============================================================
// split_commit
// ============================================================

/// Split `plan.target_oid` into `plan.parts.len()` commits.
///
/// Preconditions (all enforced as `Aborted` with a user-legible reason):
///   * `plan.parts.len() >= 2`
///   * every part's `message` is non-empty after trim
///   * the union of every part's hunks equals the commit's hunks
///     exactly — no duplicates, no missing
///   * no file is referenced by hunks in two different parts (the v1
///     "whole-file-per-part" restriction documented at the top of this
///     module); the UI surfaces this as a validation error before
///     calling us, but we re-check here as defense-in-depth
///   * the target is not a merge commit and is not the root
///
/// Guarantees: on any failure after the backup tag is created, we
/// restore the pre-split state (branch tip, working tree, stash) as
/// best we can. The backup tag itself is preserved on abort so the user
/// has an explicit recovery anchor.
///
/// Note on v2: sub-file hunk-level splitting (different hunks of the
/// same file going to different parts) is a planned enhancement but not
/// supported here. The patch-generation bookkeeping for that case is
/// large enough to justify its own module.
pub fn split_commit(repo_path: &Path, plan: SplitPlan) -> Result<SplitOutcome> {
    // ---- 1. Validate ----
    if plan.parts.len() < 2 {
        return Ok(SplitOutcome::Aborted {
            reason: "Need at least two parts to split a commit.".to_string(),
            backup_tag_created: None,
        });
    }
    for (idx, part) in plan.parts.iter().enumerate() {
        if part.message.trim().is_empty() {
            return Ok(SplitOutcome::Aborted {
                reason: format!("Part {} has an empty commit message.", idx + 1),
                backup_tag_created: None,
            });
        }
        if part.hunks.is_empty() {
            return Ok(SplitOutcome::Aborted {
                reason: format!("Part {} has no hunks assigned.", idx + 1),
                backup_tag_created: None,
            });
        }
    }

    // Enumerate the target commit's hunks.
    let all_hunks = discover_hunks(repo_path, plan.target_oid)
        .with_context(|| format!("enumerate hunks of {}", plan.target_oid))?;
    if all_hunks.is_empty() {
        return Ok(SplitOutcome::Aborted {
            reason: "Target commit has no splittable hunks (empty / binary-only diff)."
                .to_string(),
            backup_tag_created: None,
        });
    }

    if let Err(reason) = validate_plan_coverage(&plan, &all_hunks) {
        return Ok(SplitOutcome::Aborted {
            reason,
            backup_tag_created: None,
        });
    }

    // Files-per-part: guaranteed non-empty and mutually disjoint by
    // `validate_plan_coverage` above.
    let files_per_part = group_files_per_part(&plan);

    // Merge / root commit rejection.
    let parent_oid = match target_parent(repo_path, &plan.target_oid) {
        Ok(p) => p,
        Err(e) => {
            return Ok(SplitOutcome::Aborted {
                reason: e,
                backup_tag_created: None,
            })
        }
    };

    // Fetch the author identity once; reused as env vars when we
    // replay each part so every new commit gets the same authorship
    // (only committer shifts to "now").
    let (author_name, author_email, author_date) = match read_author(repo_path, &plan.target_oid) {
        Ok(t) => t,
        Err(e) => {
            return Ok(SplitOutcome::Aborted {
                reason: format!("read target author: {e:#}"),
                backup_tag_created: None,
            })
        }
    };

    // Find the current branch + HEAD (for the "replay descendants"
    // step). If the user is detached and target is not HEAD, we still
    // proceed — but there's no branch ref to move at the end.
    let branch_name = current_branch(repo_path);
    let head_oid = match super::cli::run_line(repo_path, ["rev-parse", "HEAD"]) {
        Ok(s) => match gix::ObjectId::from_hex(s.trim().as_bytes()) {
            Ok(oid) => oid,
            Err(e) => {
                return Ok(SplitOutcome::Aborted {
                    reason: format!("parse HEAD: {e}"),
                    backup_tag_created: None,
                })
            }
        },
        Err(e) => {
            return Ok(SplitOutcome::Aborted {
                reason: format!("rev-parse HEAD: {e:#}"),
                backup_tag_created: None,
            })
        }
    };

    // Target must be reachable from HEAD — otherwise the "replay
    // descendants" step has no meaning.
    if !is_ancestor(repo_path, &plan.target_oid, &head_oid) {
        return Ok(SplitOutcome::Aborted {
            reason: "Target commit is not an ancestor of HEAD; \
                     checkout the containing branch and retry."
                .to_string(),
            backup_tag_created: None,
        });
    }

    // ---- 2. Auto-stash ----
    let stashed = match auto_stash_path(repo_path, "split commit", AutoStashOpts::default()) {
        Ok(AutoStashOutcome::Clean) => false,
        Ok(AutoStashOutcome::Stashed { .. }) => true,
        Ok(AutoStashOutcome::Refused { reason }) => {
            return Ok(SplitOutcome::Aborted {
                reason: reason.to_string(),
                backup_tag_created: None,
            });
        }
        Err(e) => {
            return Ok(SplitOutcome::Aborted {
                reason: format!("auto-stash before split: {e:#}"),
                backup_tag_created: None,
            });
        }
    };

    // ---- 3. Backup tag ----
    let backup_tag = build_backup_tag_name(backup_tag_name_for("split"), now_unix_seconds());
    if let Err(e) = super::cli::run(
        repo_path,
        ["tag", "-f", "--no-sign", backup_tag.as_str(), &head_oid.to_string()],
    ) {
        let _ = maybe_pop_stash(repo_path, stashed);
        return Ok(SplitOutcome::Aborted {
            reason: format!("create backup tag: {e:#}"),
            backup_tag_created: None,
        });
    }

    // Collect the descendants of `target` up to HEAD — these are the
    // commits we'll replay after the split. `target^..HEAD` excludes
    // target itself and includes HEAD; oldest-first reversal makes
    // cherry-pick order sane. Skipping when HEAD == target.
    let descendants = if head_oid == plan.target_oid {
        Vec::new()
    } else {
        match commits_range_exclusive(repo_path, &plan.target_oid, &head_oid) {
            Ok(v) => v,
            Err(e) => {
                let _ = super::cli::run(repo_path, ["tag", "-d", backup_tag.as_str()]);
                let _ = maybe_pop_stash(repo_path, stashed);
                return Ok(SplitOutcome::Aborted {
                    reason: format!("enumerate descendants of target: {e:#}"),
                    backup_tag_created: None,
                });
            }
        }
    };

    // Helper: roll back on any post-backup-tag failure.
    let rollback = |reason: String| -> SplitOutcome {
        let _ = super::cli::run(repo_path, ["cherry-pick", "--abort"]);
        let _ = super::cli::run(repo_path, ["rebase", "--abort"]);
        let _ = super::cli::run(repo_path, ["merge", "--abort"]);
        if let Some(ref b) = branch_name {
            let _ = super::cli::run(repo_path, ["checkout", "-f", b.as_str()]);
            let _ = super::cli::run(repo_path, ["reset", "--hard", backup_tag.as_str()]);
        } else {
            let _ = super::cli::run(repo_path, ["checkout", "-f", backup_tag.as_str()]);
        }
        let _ = maybe_pop_stash(repo_path, stashed);
        SplitOutcome::Aborted {
            reason,
            backup_tag_created: Some(backup_tag.clone()),
        }
    };

    // ---- 4. Detach at parent ----
    if let Err(e) = super::cli::run(
        repo_path,
        ["checkout", "--detach", &parent_oid.to_string()],
    ) {
        return Ok(rollback(format!(
            "checkout --detach {parent_oid}: {e:#}"
        )));
    }

    // ---- 5. Apply each part in order ----
    let mut new_oids: Vec<gix::ObjectId> = Vec::with_capacity(plan.parts.len());
    for (idx, part) in plan.parts.iter().enumerate() {
        let files = &files_per_part[idx];

        // Overlay the target's version of the part's files onto the
        // index + working tree. `checkout <tree> -- <paths>` handles
        // adds / modifies / deletes (a file deleted in target vs.
        // parent becomes `rm`'d on checkout). We pass `--` so paths
        // starting with `-` aren't parsed as flags.
        let mut args: Vec<String> = vec![
            "checkout".to_string(),
            plan.target_oid.to_string(),
            "--".to_string(),
        ];
        for f in files {
            match f.to_str() {
                Some(s) => args.push(s.to_string()),
                None => {
                    return Ok(rollback(format!(
                        "part {} file {:?} is not valid UTF-8",
                        idx + 1,
                        f
                    )));
                }
            }
        }
        if let Err(e) = super::cli::run(repo_path, args.iter().map(String::as_str)) {
            return Ok(rollback(format!(
                "checkout {} files for part {}: {e:#}",
                files.len(),
                idx + 1
            )));
        }

        // `git checkout <tree> -- path` for a file that is deleted in
        // `<tree>` silently no-ops on some git versions instead of
        // removing the working-tree file. Detect that by comparing the
        // target-tree and parent-tree presence; if the file exists in
        // parent but not in target, explicitly `rm`.
        for f in files {
            let in_target = path_exists_in_commit(repo_path, &plan.target_oid, f);
            if !in_target {
                let s = match f.to_str() {
                    Some(s) => s,
                    None => continue,
                };
                // Ignore errors — `rm -- <missing>` fails harmlessly.
                let _ = super::cli::run(repo_path, ["rm", "-f", "--quiet", "--", s]);
            }
        }

        // Commit with the preserved author identity. `--allow-empty`
        // guards against the edge case where two semantically
        // different hunks on the same file produced a net-zero change
        // on the subset we picked (extremely rare with whole-file
        // granularity, but still safer than silently dropping the
        // commit).
        let commit_res = super::cli::GitCommand::new(repo_path)
            .args([
                "commit",
                "--allow-empty",
                "--no-verify",
                "-F",
                "-",
            ])
            .stdin(part.message.as_bytes().to_vec())
            .env("GIT_AUTHOR_NAME", &author_name)
            .env("GIT_AUTHOR_EMAIL", &author_email)
            .env("GIT_AUTHOR_DATE", &author_date)
            .run();
        if let Err(e) = commit_res {
            return Ok(rollback(format!(
                "commit part {}: {e:#}",
                idx + 1
            )));
        }

        let new_oid = match super::cli::run_line(repo_path, ["rev-parse", "HEAD"]) {
            Ok(s) => match gix::ObjectId::from_hex(s.trim().as_bytes()) {
                Ok(oid) => oid,
                Err(e) => {
                    return Ok(rollback(format!("parse part {} oid: {e}", idx + 1)));
                }
            },
            Err(e) => {
                return Ok(rollback(format!(
                    "rev-parse HEAD after part {}: {e:#}",
                    idx + 1
                )));
            }
        };
        new_oids.push(new_oid);
    }

    // After all parts, verify the accumulated tree equals the target
    // tree. If it doesn't, the split was lossy (shouldn't happen under
    // whole-file granularity with exhaustive coverage, but the
    // check is cheap and catches future regressions).
    if let Err(mismatch) = assert_trees_match(repo_path, &plan.target_oid) {
        return Ok(rollback(format!(
            "split produced a tree that doesn't match the target commit: {mismatch}"
        )));
    }

    // ---- 6. Replay descendants on top of the split tip ----
    for oid in &descendants {
        let output = super::cli::GitCommand::new(repo_path)
            .args(["cherry-pick", "--allow-empty", &oid.to_string()])
            .run_raw();
        let ok = matches!(&output, Ok(o) if o.status.success());
        if !ok {
            return Ok(rollback(format!(
                "cherry-pick {} during descendant replay conflicted",
                short_oid(oid)
            )));
        }
    }

    // ---- 7. Move the branch ref & pop the stash ----
    let new_head_oid = match super::cli::run_line(repo_path, ["rev-parse", "HEAD"]) {
        Ok(s) => match gix::ObjectId::from_hex(s.trim().as_bytes()) {
            Ok(oid) => oid,
            Err(e) => return Ok(rollback(format!("parse new HEAD: {e}"))),
        },
        Err(e) => return Ok(rollback(format!("rev-parse new HEAD: {e:#}"))),
    };

    if let Some(ref b) = branch_name {
        if let Err(e) = super::cli::run(
            repo_path,
            ["branch", "-f", b.as_str(), &new_head_oid.to_string()],
        ) {
            return Ok(rollback(format!("branch -f {b}: {e:#}")));
        }
        if let Err(e) = super::cli::run(repo_path, ["checkout", b.as_str()]) {
            return Ok(rollback(format!("checkout {b}: {e:#}")));
        }
    }

    if stashed {
        let _ = super::cli::run(repo_path, ["stash", "pop"]);
    }

    Ok(SplitOutcome::Success {
        new_head_oid,
        new_commit_oids: new_oids,
        backup_tag,
    })
}

/// Ensure every original hunk is assigned to exactly one part and no
/// file straddles parts (v1 whole-file-per-part restriction).
fn validate_plan_coverage(
    plan: &SplitPlan,
    all_hunks: &[DiscoveredHunk],
) -> std::result::Result<(), String> {
    // Build the canonical set of (file, hunk_index) tuples from the
    // real commit.
    let mut canonical: BTreeSet<(PathBuf, usize)> = BTreeSet::new();
    for h in all_hunks {
        canonical.insert((h.file.clone(), h.hunk_index));
    }

    let mut seen: BTreeMap<(PathBuf, usize), usize> = BTreeMap::new();
    let mut file_to_part: BTreeMap<PathBuf, usize> = BTreeMap::new();

    for (part_idx, part) in plan.parts.iter().enumerate() {
        for hunk in &part.hunks {
            let key = (hunk.file.clone(), hunk.hunk_index);
            if !canonical.contains(&key) {
                return Err(format!(
                    "Part {} references hunk {} of {:?}, which does not exist in the target commit.",
                    part_idx + 1,
                    hunk.hunk_index,
                    hunk.file
                ));
            }
            if let Some(prev) = seen.insert(key.clone(), part_idx) {
                return Err(format!(
                    "Hunk {} of {:?} is assigned to both part {} and part {}.",
                    hunk.hunk_index,
                    hunk.file,
                    prev + 1,
                    part_idx + 1
                ));
            }
            match file_to_part.get(&hunk.file).copied() {
                Some(existing) if existing != part_idx => {
                    return Err(format!(
                        "File {:?} has hunks in both part {} and part {}. v1 split requires all \
                         hunks of a single file to land in the same part.",
                        hunk.file,
                        existing + 1,
                        part_idx + 1
                    ));
                }
                _ => {
                    file_to_part.insert(hunk.file.clone(), part_idx);
                }
            }
        }
    }

    // Every canonical hunk must be covered.
    for key in &canonical {
        if !seen.contains_key(key) {
            return Err(format!(
                "Hunk {} of {:?} is not assigned to any part.",
                key.1, key.0
            ));
        }
    }
    Ok(())
}

/// For each part (in order), the list of files it owns. Uses
/// first-appearance order within the part for determinism.
fn group_files_per_part(plan: &SplitPlan) -> Vec<Vec<PathBuf>> {
    let mut out: Vec<Vec<PathBuf>> = Vec::with_capacity(plan.parts.len());
    for part in &plan.parts {
        let mut seen = BTreeSet::new();
        let mut ordered: Vec<PathBuf> = Vec::new();
        for h in &part.hunks {
            if seen.insert(h.file.clone()) {
                ordered.push(h.file.clone());
            }
        }
        out.push(ordered);
    }
    out
}

// ============================================================
// Small helpers
// ============================================================

fn target_parent(
    repo_path: &Path,
    target: &gix::ObjectId,
) -> std::result::Result<gix::ObjectId, String> {
    let out = super::cli::run(
        repo_path,
        ["rev-list", "--parents", "--no-walk", &target.to_string()],
    )
    .map_err(|e| format!("rev-list --parents {target}: {e:#}"))?;
    let text = out.stdout_str();
    let line = text.lines().next().unwrap_or("").trim();
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return Err("Cannot split a root commit (no parent to rebuild on top of).".to_string());
    }
    if parts.len() > 2 {
        return Err("Cannot split a merge commit (multiple parents).".to_string());
    }
    gix::ObjectId::from_hex(parts[1].as_bytes()).map_err(|e| format!("parse parent oid: {e}"))
}

fn read_author(
    repo_path: &Path,
    target: &gix::ObjectId,
) -> Result<(String, String, String)> {
    let out = super::cli::run(
        repo_path,
        ["show", "-s", "--format=%an%n%ae%n%aI", &target.to_string()],
    )
    .with_context(|| format!("read author of {target}"))?;
    let text = out.stdout_str();
    let mut it = text.lines();
    let name = it.next().unwrap_or("").to_string();
    let email = it.next().unwrap_or("").to_string();
    let date = it.next().unwrap_or("").to_string();
    Ok((name, email, date))
}

fn current_branch(repo_path: &Path) -> Option<String> {
    let line = super::cli::run_line(repo_path, ["symbolic-ref", "--short", "HEAD"]).ok()?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn is_ancestor(repo_path: &Path, maybe_ancestor: &gix::ObjectId, descendant: &gix::ObjectId) -> bool {
    if maybe_ancestor == descendant {
        return true;
    }
    match super::cli::GitCommand::new(repo_path)
        .args([
            "merge-base",
            "--is-ancestor",
            &maybe_ancestor.to_string(),
            &descendant.to_string(),
        ])
        .run_raw()
    {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

/// Enumerate commits in `(from, to]` in oldest-first order so the
/// caller can cherry-pick them in apply order. Excludes `from`,
/// includes `to`.
fn commits_range_exclusive(
    repo_path: &Path,
    from: &gix::ObjectId,
    to: &gix::ObjectId,
) -> Result<Vec<gix::ObjectId>> {
    let range = format!("{}..{}", from, to);
    let out = super::cli::run(
        repo_path,
        ["rev-list", "--reverse", "--topo-order", range.as_str()],
    )
    .context("rev-list range")?;
    let mut v = Vec::new();
    for line in out.stdout_str().lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let oid =
            gix::ObjectId::from_hex(trimmed.as_bytes()).context("parse range oid")?;
        v.push(oid);
    }
    Ok(v)
}

fn path_exists_in_commit(repo_path: &Path, commit: &gix::ObjectId, path: &Path) -> bool {
    let s = match path.to_str() {
        Some(s) => s,
        None => return false,
    };
    let spec = format!("{}:{}", commit, s);
    super::cli::GitCommand::new(repo_path)
        .args(["cat-file", "-e", spec.as_str()])
        .run_raw()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Assert that HEAD's tree matches `expected`'s tree. Returns an error
/// describing the divergence if they differ.
fn assert_trees_match(
    repo_path: &Path,
    expected: &gix::ObjectId,
) -> std::result::Result<(), String> {
    let head_tree = super::cli::run_line(repo_path, ["rev-parse", "HEAD^{tree}"])
        .map_err(|e| format!("rev-parse HEAD^{{tree}}: {e:#}"))?;
    let expected_tree = super::cli::run_line(
        repo_path,
        ["rev-parse", &format!("{}^{{tree}}", expected)],
    )
    .map_err(|e| format!("rev-parse {}^{{tree}}: {e:#}", expected))?;
    if head_tree.trim() == expected_tree.trim() {
        Ok(())
    } else {
        Err(format!(
            "HEAD tree {} != target tree {}",
            head_tree.trim(),
            expected_tree.trim()
        ))
    }
}

fn maybe_pop_stash(repo_path: &Path, stashed: bool) {
    if stashed {
        let _ = super::cli::run(repo_path, ["stash", "pop"]);
    }
}

fn short_oid(oid: &gix::ObjectId) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static UNIQ: AtomicU64 = AtomicU64::new(0);

    fn unique_dir(tag: &str) -> PathBuf {
        let n = UNIQ.fetch_add(1, Ordering::Relaxed);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "mergefox-split-test-{}-{}-{}-{n}",
            tag,
            std::process::id(),
            ts
        ))
    }

    /// Create a bare-ish temp repo with `user.email` / `user.name`
    /// configured so `git commit` doesn't fail in CI sandboxes.
    fn init_repo(tag: &str) -> PathBuf {
        let dir = unique_dir(tag);
        std::fs::create_dir_all(&dir).unwrap();
        super::super::cli::run(&dir, ["init", "-q", "-b", "main"]).unwrap();
        super::super::cli::run(&dir, ["config", "user.email", "test@mergefox.local"]).unwrap();
        super::super::cli::run(&dir, ["config", "user.name", "Split Tester"]).unwrap();
        super::super::cli::run(&dir, ["config", "commit.gpgsign", "false"]).unwrap();
        super::super::cli::run(&dir, ["config", "tag.gpgsign", "false"]).unwrap();
        dir
    }

    fn write_file(dir: &Path, rel: &str, contents: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, contents).unwrap();
    }

    fn commit_all(dir: &Path, msg: &str) -> gix::ObjectId {
        super::super::cli::run(dir, ["add", "-A"]).unwrap();
        super::super::cli::run(dir, ["commit", "-q", "--no-verify", "-m", msg]).unwrap();
        let s = super::super::cli::run_line(dir, ["rev-parse", "HEAD"]).unwrap();
        gix::ObjectId::from_hex(s.trim().as_bytes()).unwrap()
    }

    #[test]
    fn backup_tag_name_is_iso_ish() {
        assert_eq!(
            build_backup_tag_name("split", 0),
            "mergefox/split/19700101T000000Z"
        );
    }

    #[test]
    fn parse_hunk_header_simple() {
        let h = parse_hunk_header("@@ -10,3 +10,4 @@ fn foo()").unwrap();
        assert_eq!(h.old_start, 10);
        assert_eq!(h.old_count, 3);
        assert_eq!(h.new_start, 10);
        assert_eq!(h.new_count, 4);
    }

    #[test]
    fn parse_hunk_header_rejects_non_hunks() {
        assert!(parse_hunk_header("diff --git a.rs b.rs").is_none());
        assert!(parse_hunk_header("+added line").is_none());
    }

    #[test]
    fn parse_hunk_header_single_line_defaults_count_to_one() {
        let h = parse_hunk_header("@@ -7 +7 @@").unwrap();
        assert_eq!(h.old_count, 1);
        assert_eq!(h.new_count, 1);
    }

    #[test]
    fn parse_diff_git_picks_new_path() {
        // With --no-prefix, both sides are the same plain path.
        let p = parse_diff_git_paths("foo.rs foo.rs").unwrap();
        assert_eq!(p, PathBuf::from("foo.rs"));
    }

    #[test]
    fn parse_diff_git_handles_rename() {
        let p = parse_diff_git_paths("old/file.rs new/file.rs").unwrap();
        assert_eq!(p, PathBuf::from("new/file.rs"));
    }

    #[test]
    fn parse_unified_diff_preserves_hunk_order_within_file() {
        // Two hunks in the same file — we expect them in the order
        // they appear.
        let text = "\
diff --git a.rs a.rs
--- a.rs
+++ a.rs
@@ -1,2 +1,2 @@
-old1
+new1
 ctx
@@ -20,2 +20,2 @@
-old2
+new2
 ctx
";
        let hunks = parse_unified_diff_hunks(text);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].hunk_index, 0);
        assert_eq!(hunks[1].hunk_index, 1);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[1].old_start, 20);
        assert_eq!(hunks[0].file, PathBuf::from("a.rs"));
        assert_eq!(hunks[1].file, PathBuf::from("a.rs"));
    }

    #[test]
    fn parse_unified_diff_resets_hunk_index_per_file() {
        let text = "\
diff --git a.rs a.rs
--- a.rs
+++ a.rs
@@ -1,1 +1,1 @@
-a
+A
diff --git b.rs b.rs
--- b.rs
+++ b.rs
@@ -1,1 +1,1 @@
-b
+B
";
        let hunks = parse_unified_diff_hunks(text);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].file, PathBuf::from("a.rs"));
        assert_eq!(hunks[0].hunk_index, 0);
        assert_eq!(hunks[1].file, PathBuf::from("b.rs"));
        assert_eq!(hunks[1].hunk_index, 0);
    }

    #[test]
    fn split_rejects_single_part() {
        let dir = init_repo("single-part");
        write_file(&dir, "a.txt", "one\n");
        let _c0 = commit_all(&dir, "root");
        write_file(&dir, "a.txt", "one\ntwo\n");
        let target = commit_all(&dir, "add line");

        let plan = SplitPlan {
            target_oid: target,
            parts: vec![SplitPart {
                message: "only one".into(),
                hunks: vec![HunkRef {
                    file: PathBuf::from("a.txt"),
                    hunk_index: 0,
                }],
            }],
        };
        let out = split_commit(&dir, plan).unwrap();
        match out {
            SplitOutcome::Aborted { reason, .. } => {
                assert!(reason.to_lowercase().contains("two"), "reason={reason}");
            }
            _ => panic!("expected Aborted"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_rejects_missing_hunk() {
        // Target commit has two hunks (in separate files); plan
        // assigns only one of them → coverage check flags the other
        // as "not assigned".
        let dir = init_repo("missing-hunk");
        write_file(&dir, "a.txt", "a1\n");
        write_file(&dir, "b.txt", "b1\n");
        let _c0 = commit_all(&dir, "root");
        write_file(&dir, "a.txt", "a1\na2\n");
        write_file(&dir, "b.txt", "b1\nb2\n");
        let target = commit_all(&dir, "edit both");

        let plan = SplitPlan {
            target_oid: target,
            parts: vec![
                SplitPart {
                    message: "part1".into(),
                    hunks: vec![HunkRef {
                        file: PathBuf::from("a.txt"),
                        hunk_index: 0,
                    }],
                },
                SplitPart {
                    message: "part2".into(),
                    // Deliberately assigning the same hunk again instead
                    // of the missing one from b.txt. Triggers either
                    // "already assigned" or "not assigned" errors — both
                    // are abort-worthy.
                    hunks: vec![HunkRef {
                        file: PathBuf::from("a.txt"),
                        hunk_index: 0,
                    }],
                },
            ],
        };
        let out = split_commit(&dir, plan).unwrap();
        assert!(matches!(out, SplitOutcome::Aborted { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_hunks_finds_every_hunk_per_file() {
        let dir = init_repo("discover");
        write_file(&dir, "foo.txt", "l1\nl2\nl3\nl4\nl5\nl6\n");
        write_file(&dir, "bar.txt", "b1\nb2\nb3\n");
        let _c0 = commit_all(&dir, "root");

        // Edit foo.txt in two non-adjacent regions (producing two
        // hunks) and bar.txt in one region.
        write_file(&dir, "foo.txt", "L1\nl2\nl3\nl4\nl5\nL6\n");
        write_file(&dir, "bar.txt", "b1\nB2\nb3\n");
        let target = commit_all(&dir, "multi-hunk edit");

        let hunks = discover_hunks(&dir, target).unwrap();
        // foo.txt might yield 1 or 2 hunks depending on diff context
        // (6-line file with context=3 often merges). We make the
        // assertion narrower: >= 2 total across both files, and every
        // hunk's file is one of the expected two.
        assert!(hunks.len() >= 2, "got {} hunks: {hunks:#?}", hunks.len());
        for h in &hunks {
            assert!(
                h.file == PathBuf::from("foo.txt") || h.file == PathBuf::from("bar.txt"),
                "unexpected file {:?}",
                h.file
            );
            assert!(h.header.starts_with("@@ "));
        }
        // hunk_index monotonically increases within each file and
        // starts at 0.
        let mut per_file: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        for h in &hunks {
            per_file.entry(h.file.clone()).or_default().push(h.hunk_index);
        }
        for (_f, indices) in per_file {
            assert_eq!(indices[0], 0);
            for win in indices.windows(2) {
                assert_eq!(win[1], win[0] + 1);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_rejects_file_across_parts() {
        // Both parts claim hunk 0 of the same file; the first put
        // wins, the second triggers the "already assigned" or
        // "file straddles parts" check — which is exactly the v1
        // restriction we want to lock in.
        let dir = init_repo("straddle");
        write_file(&dir, "a.txt", "1\n2\n");
        let _c0 = commit_all(&dir, "root");
        write_file(&dir, "a.txt", "1\n2\n3\n");
        let target = commit_all(&dir, "append");

        let plan = SplitPlan {
            target_oid: target,
            parts: vec![
                SplitPart {
                    message: "p1".into(),
                    hunks: vec![HunkRef {
                        file: PathBuf::from("a.txt"),
                        hunk_index: 0,
                    }],
                },
                SplitPart {
                    message: "p2".into(),
                    hunks: vec![HunkRef {
                        file: PathBuf::from("a.txt"),
                        hunk_index: 0,
                    }],
                },
            ],
        };
        let out = split_commit(&dir, plan).unwrap();
        assert!(matches!(out, SplitOutcome::Aborted { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_two_files_into_two_parts_preserves_tree() {
        // End-to-end round trip: one commit touches two files; split
        // into two parts, one file each; the resulting tip must have
        // the same tree as the original commit.
        let dir = init_repo("roundtrip-2x1");
        write_file(&dir, "a.txt", "A0\n");
        write_file(&dir, "b.txt", "B0\n");
        let _c0 = commit_all(&dir, "root");
        write_file(&dir, "a.txt", "A0\nA1\n");
        write_file(&dir, "b.txt", "B0\nB1\n");
        let target = commit_all(&dir, "edit both files");

        let hunks = discover_hunks(&dir, target).unwrap();
        // One hunk per file (append at EOF).
        assert!(hunks.iter().any(|h| h.file == PathBuf::from("a.txt")));
        assert!(hunks.iter().any(|h| h.file == PathBuf::from("b.txt")));

        let a_hunks: Vec<HunkRef> = hunks
            .iter()
            .filter(|h| h.file == PathBuf::from("a.txt"))
            .map(|h| HunkRef {
                file: h.file.clone(),
                hunk_index: h.hunk_index,
            })
            .collect();
        let b_hunks: Vec<HunkRef> = hunks
            .iter()
            .filter(|h| h.file == PathBuf::from("b.txt"))
            .map(|h| HunkRef {
                file: h.file.clone(),
                hunk_index: h.hunk_index,
            })
            .collect();

        let plan = SplitPlan {
            target_oid: target,
            parts: vec![
                SplitPart {
                    message: "edit a".into(),
                    hunks: a_hunks,
                },
                SplitPart {
                    message: "edit b".into(),
                    hunks: b_hunks,
                },
            ],
        };
        let out = split_commit(&dir, plan).unwrap();
        let (new_head_oid, new_commit_oids, _backup) = match out {
            SplitOutcome::Success {
                new_head_oid,
                new_commit_oids,
                backup_tag,
            } => (new_head_oid, new_commit_oids, backup_tag),
            SplitOutcome::Aborted { reason, .. } => panic!("unexpected abort: {reason}"),
        };
        assert_eq!(new_commit_oids.len(), 2);

        // Tree at the new head must equal the tree at the original
        // target. If it doesn't, the split was lossy.
        let head_tree = super::super::cli::run_line(&dir, ["rev-parse", "HEAD^{tree}"])
            .unwrap()
            .trim()
            .to_string();
        let target_tree =
            super::super::cli::run_line(&dir, ["rev-parse", &format!("{target}^{{tree}}")])
                .unwrap()
                .trim()
                .to_string();
        assert_eq!(
            head_tree, target_tree,
            "resulting tree should equal target tree"
        );
        assert_ne!(new_head_oid, target, "new head should not be target");

        // Also: two commits ahead of root.
        let count = super::super::cli::run_line(&dir, ["rev-list", "--count", "HEAD"]).unwrap();
        assert_eq!(count.trim(), "3");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_preserves_author_identity() {
        let dir = init_repo("preserve-author");
        // Override author explicitly on the target commit so we can
        // verify the split commits inherit it (instead of whatever the
        // config says).
        write_file(&dir, "a.txt", "a\n");
        write_file(&dir, "b.txt", "b\n");
        super::super::cli::run(&dir, ["add", "-A"]).unwrap();
        super::super::cli::run(
            &dir,
            [
                "commit",
                "-q",
                "--no-verify",
                "--author",
                "Target Author <target@example.com>",
                "-m",
                "root",
            ],
        )
        .unwrap();

        write_file(&dir, "a.txt", "a\nA2\n");
        write_file(&dir, "b.txt", "b\nB2\n");
        super::super::cli::run(&dir, ["add", "-A"]).unwrap();
        super::super::cli::run(
            &dir,
            [
                "commit",
                "-q",
                "--no-verify",
                "--author",
                "Target Author <target@example.com>",
                "-m",
                "edit both",
            ],
        )
        .unwrap();
        let target = gix::ObjectId::from_hex(
            super::super::cli::run_line(&dir, ["rev-parse", "HEAD"])
                .unwrap()
                .trim()
                .as_bytes(),
        )
        .unwrap();

        let hunks = discover_hunks(&dir, target).unwrap();
        let plan = SplitPlan {
            target_oid: target,
            parts: vec![
                SplitPart {
                    message: "p1 (a)".into(),
                    hunks: hunks
                        .iter()
                        .filter(|h| h.file == PathBuf::from("a.txt"))
                        .map(|h| HunkRef {
                            file: h.file.clone(),
                            hunk_index: h.hunk_index,
                        })
                        .collect(),
                },
                SplitPart {
                    message: "p2 (b)".into(),
                    hunks: hunks
                        .iter()
                        .filter(|h| h.file == PathBuf::from("b.txt"))
                        .map(|h| HunkRef {
                            file: h.file.clone(),
                            hunk_index: h.hunk_index,
                        })
                        .collect(),
                },
            ],
        };
        let out = split_commit(&dir, plan).unwrap();
        assert!(matches!(out, SplitOutcome::Success { .. }), "{:?}", out);

        // HEAD^ and HEAD should both have the target author.
        let a_head = super::super::cli::run_line(&dir, ["log", "-1", "--format=%ae", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        let a_parent = super::super::cli::run_line(&dir, ["log", "-1", "--format=%ae", "HEAD^"])
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(a_head, "target@example.com");
        assert_eq!(a_parent, "target@example.com");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_plan_catches_unknown_hunk() {
        let canonical = vec![DiscoveredHunk {
            file: PathBuf::from("a.txt"),
            hunk_index: 0,
            header: "@@ -1 +1 @@".into(),
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            preview: String::new(),
        }];
        let oid = gix::ObjectId::null(gix::hash::Kind::Sha1);
        let plan = SplitPlan {
            target_oid: oid,
            parts: vec![
                SplitPart {
                    message: "p1".into(),
                    hunks: vec![HunkRef {
                        file: PathBuf::from("a.txt"),
                        hunk_index: 0,
                    }],
                },
                SplitPart {
                    message: "p2".into(),
                    // Invented hunk index that isn't in canonical.
                    hunks: vec![HunkRef {
                        file: PathBuf::from("a.txt"),
                        hunk_index: 99,
                    }],
                },
            ],
        };
        let err = validate_plan_coverage(&plan, &canonical).unwrap_err();
        assert!(err.contains("does not exist"), "err={err}");
    }

    #[test]
    fn civil_handles_known_dates() {
        assert_eq!(civil_from_unix_seconds(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(
            civil_from_unix_seconds(951_782_400),
            (2000, 2, 29, 0, 0, 0)
        );
    }
}
