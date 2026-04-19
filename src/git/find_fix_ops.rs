//! Find-and-fix across working tree + commit history.
//!
//! Motivation
//! ----------
//! A common real-world workflow is "I need to scrub a term from both
//! my uncommitted code and every commit message that mentions it" —
//! a one-go replacement that otherwise requires
//!
//!   1. `git grep` + a manual find/replace across N files, then a
//!      commit.
//!   2. `git log --grep=<term>` to list offending commits, then
//!      `git rebase -i` + marking every hit as `reword` + an editor
//!      dance per commit.
//!
//! This module bundles the two into a single transactional operation
//! sharing one backup tag + auto-stash envelope. All mutations live
//! behind `apply` — `scan` is read-only and safe to run whenever the
//! user types in the search box.
//!
//! Scope (v1)
//! ----------
//! * Literal-string search only (no regex — deliberately simpler;
//!   easier to reason about, easier to review in the modal, and
//!   avoids having to land ripgrep). Regex can follow.
//! * Working-tree scope scans tracked files via `git grep -n`. We
//!   skip binary files and submodules because git itself does, so
//!   the scan results are already "things a human would expect to
//!   see in a grep".
//! * Commit-message scope iterates `git log --all --format=…` and
//!   matches against subject + body. We *deliberately* don't scan
//!   the diff (`-G<pattern>`) — a term that only appears in a diff
//!   line isn't "in the commit", it's in the CODE and handled by the
//!   working-tree scope on its current checkout.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::repo::{auto_stash_path, AutoStashOpts, AutoStashOutcome};
use super::reword_ops::reword_commit;

/// A single hit inside a working-tree file. Used by the UI to render
/// the pre-apply preview list; carries just enough to display the
/// line and compute a 1:1 literal replacement when the user confirms.
#[derive(Debug, Clone)]
pub struct WorkingTreeMatch {
    /// Repo-relative path — what `git grep` emits.
    pub path: PathBuf,
    /// 1-based line number, matching every editor / `git blame`
    /// format the user has already seen.
    pub line_number: u32,
    /// The whole line verbatim (no trailing newline). Ready to drop
    /// into the modal as-is.
    pub line: String,
    /// Position of the search hit within `line` (byte index), so the
    /// UI can highlight it.
    pub match_start: usize,
    pub match_end: usize,
}

/// A hit inside a commit's message — either the subject line or
/// somewhere in the body. The UI uses `oid` as the stable identifier
/// even though the OID will rewrite on apply; we re-look up the new
/// OID after the batch reword finishes and report the mapping back.
#[derive(Debug, Clone)]
pub struct CommitMatch {
    pub oid: gix::ObjectId,
    pub subject: String,
    /// True if the hit is on the subject line; false if body-only.
    /// Both can be true (we record the commit once if either hits).
    pub subject_hit: bool,
    pub body_hit: bool,
}

/// Everything the user picked from the results list plus the
/// replacement string. Passed to `apply` as an atomic unit so the
/// backup-tag + auto-stash envelope covers the whole batch.
#[derive(Debug, Clone)]
pub struct ApplyPlan {
    pub pattern: String,
    pub replacement: String,
    /// Subset of paths from `scan.working_tree` the user ticked for
    /// apply. Files not in this list are left alone even if they
    /// contained matches.
    pub apply_working_tree_paths: Vec<PathBuf>,
    /// Subset of commits the user ticked. Each gets its message
    /// rewritten via the standard reword flow.
    pub apply_commit_oids: Vec<gix::ObjectId>,
}

/// Outcome of `apply` — rich enough that the UI can render a "here's
/// what landed" summary without an extra round-trip, and structured
/// enough that the MCP tool wrapper can produce a dry-run preview
/// from the same shape.
#[derive(Debug, Clone)]
pub enum ApplyOutcome {
    /// Everything applied cleanly. `working_tree_files_changed` is
    /// the count of distinct paths written; `commit_oid_remap` lists
    /// (old, new) for each reworded commit in topological order so
    /// the UI can repoint selections.
    Success {
        working_tree_files_changed: usize,
        commit_oid_remap: Vec<(gix::ObjectId, gix::ObjectId)>,
        backup_tag: Option<String>,
        auto_stashed: bool,
    },
    /// A step failed mid-apply. The envelope rolled back to the
    /// backup tag + restored the stash; the repo is back to where it
    /// started modulo the tag itself (which we leave for manual
    /// recovery). `reason` is suitable for an error toast detail.
    Aborted {
        reason: String,
        backup_tag: Option<String>,
    },
}

/// Read-only scan. Safe to invoke on every keystroke from the modal
/// — the working-tree grep is a single subprocess and the log walk
/// costs O(commits × average_message_bytes), both fast enough at
/// repo scales we care about.
pub fn scan(
    repo_path: &Path,
    pattern: &str,
    include_working_tree: bool,
    include_commit_messages: bool,
    commit_history_limit: usize,
) -> Result<ScanResult> {
    if pattern.is_empty() {
        return Ok(ScanResult::default());
    }
    let mut out = ScanResult::default();
    if include_working_tree {
        out.working_tree = scan_working_tree(repo_path, pattern)?;
    }
    if include_commit_messages {
        out.commit_messages = scan_commit_messages(repo_path, pattern, commit_history_limit)?;
    }
    Ok(out)
}

/// Everything `scan` found. Separate struct (rather than two Vecs)
/// so future additions (file-path scope, tag-name scope) can grow
/// without cascading signature changes.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub working_tree: Vec<WorkingTreeMatch>,
    pub commit_messages: Vec<CommitMatch>,
}

fn scan_working_tree(repo_path: &Path, pattern: &str) -> Result<Vec<WorkingTreeMatch>> {
    // `git grep -F -n -I -z --no-color --untracked <pat>` —
    //   -F = fixed-string (literal, no regex surprises)
    //   -n = line number
    //   -I = skip binary files
    //   -z = NUL-separated output so paths with quotes / CR etc.
    //        come through intact
    //   --untracked = also search uncommitted new files so the user
    //        can scrub a term they just typed but haven't committed
    let output = super::cli::GitCommand::new(repo_path)
        .args([
            "grep",
            "-F",
            "-n",
            "-I",
            "-z",
            "--no-color",
            "--untracked",
            "-e",
            pattern,
        ])
        .run_raw()
        .context("spawn git grep")?;
    // git grep exits 1 when there are no matches — that's not an error.
    let code = output.status.code().unwrap_or(-1);
    if code != 0 && code != 1 {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            return Ok(Vec::new());
        }
        anyhow::bail!("git grep failed: {stderr}");
    }
    Ok(parse_grep_z_output(&output.stdout, pattern))
}

/// Parse `git grep -z` output. With `-z` the separator between
/// `path`, `line`, and `match` fields is `\0` rather than `:`, which
/// makes the parse trivial and robust to paths that contain `:`.
fn parse_grep_z_output(data: &[u8], pattern: &str) -> Vec<WorkingTreeMatch> {
    let mut out = Vec::new();
    // Records are `path\0lineno\0line\n`. We split first on `\n` to
    // get per-record chunks, then on `\0` to pull path + line number
    // + line content out.
    let text = String::from_utf8_lossy(data);
    for record in text.split('\n') {
        if record.is_empty() {
            continue;
        }
        let parts: Vec<&str> = record.splitn(3, '\0').collect();
        if parts.len() != 3 {
            continue;
        }
        let path = PathBuf::from(parts[0]);
        let line_number = match parts[1].parse::<u32>() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let line = parts[2].to_string();
        // Compute the first match's byte range so the UI can
        // highlight it. `git grep` itself guarantees at least one
        // match exists on the line; `find` returning None here
        // would indicate a caller / encoding bug we'd rather
        // surface than paper over.
        let Some(start) = line.find(pattern) else {
            continue;
        };
        let end = start + pattern.len();
        out.push(WorkingTreeMatch {
            path,
            line_number,
            line,
            match_start: start,
            match_end: end,
        });
    }
    out
}

fn scan_commit_messages(
    repo_path: &Path,
    pattern: &str,
    limit: usize,
) -> Result<Vec<CommitMatch>> {
    // Walk commits via plain `git log`. We deliberately pin to the
    // current branch (not `--all`) because find-and-fix rewrites
    // history, and rewriting across branch tips you don't own is a
    // separate, higher-risk operation.
    //
    // Record format uses ASCII US/RS so parsing doesn't wrestle with
    // newlines inside commit bodies:
    //   <oid>\x1f<subject>\x1f<body>\x1e
    let fmt = format!("%H\x1f%s\x1f%b\x1e");
    let mut args = vec![
        "log".to_string(),
        format!("--pretty=format:{fmt}"),
        format!("-n{limit}"),
    ];
    // Empty commits with no parents don't have a `--not <none>` guard
    // needed — `log` handles them fine.
    args.push("HEAD".to_string());
    let output = super::cli::run(repo_path, args.iter().map(String::as_str))
        .context("git log for commit-message scan")?;
    let text = output.stdout_str();
    let mut out = Vec::new();
    for record in text.split('\x1e') {
        let record = record.trim_start_matches('\n');
        if record.is_empty() {
            continue;
        }
        let parts: Vec<&str> = record.splitn(3, '\x1f').collect();
        if parts.len() != 3 {
            continue;
        }
        let oid = match gix::ObjectId::from_hex(parts[0].trim().as_bytes()) {
            Ok(o) => o,
            Err(_) => continue,
        };
        let subject = parts[1].to_string();
        let body = parts[2];
        let subject_hit = subject.contains(pattern);
        let body_hit = body.contains(pattern);
        if subject_hit || body_hit {
            out.push(CommitMatch {
                oid,
                subject,
                subject_hit,
                body_hit,
            });
        }
    }
    Ok(out)
}

/// Apply the plan. Transactional: backup tag + auto-stash, then
/// working-tree replacements, then batch message rewrites in a single
/// rebase pass (oldest first). Anything goes wrong → roll back to the
/// backup tag + restore the stash.
pub fn apply(repo_path: &Path, plan: ApplyPlan) -> Result<ApplyOutcome> {
    if plan.pattern.is_empty() {
        return Ok(ApplyOutcome::Aborted {
            reason: "empty search pattern".into(),
            backup_tag: None,
        });
    }
    if plan.apply_working_tree_paths.is_empty() && plan.apply_commit_oids.is_empty() {
        return Ok(ApplyOutcome::Aborted {
            reason: "nothing selected to apply".into(),
            backup_tag: None,
        });
    }

    // Auto-stash first so the working-tree rewrite step starts from a
    // clean slate. `auto_stash_path` is a no-op if the tree is already
    // clean, so we always call it and branch on the variant.
    let auto_stashed = match auto_stash_path(
        repo_path,
        "find-and-fix across history",
        AutoStashOpts::default(),
    )
    .context("auto-stash before find-and-fix")?
    {
        AutoStashOutcome::Clean => false,
        AutoStashOutcome::Stashed { .. } => true,
        AutoStashOutcome::Refused { reason } => {
            return Ok(ApplyOutcome::Aborted {
                reason: reason.to_string(),
                backup_tag: None,
            });
        }
    };

    let backup_tag = crate::git::reword_ops::backup_tag_name_for("findfix");
    let tag_created = super::cli::run(
        repo_path,
        ["tag", "-f", "--no-sign", &backup_tag, "HEAD"],
    )
    .is_ok();
    let backup_tag_opt = if tag_created { Some(backup_tag.clone()) } else { None };

    // Rollback closure — fires on any downstream error. Kept inline so
    // we don't have to thread a `?` + early-return dance through every
    // call; instead each step matches and hands control to rollback.
    let rollback = |reason: String| -> Result<ApplyOutcome> {
        let _ = super::cli::run(repo_path, ["rebase", "--abort"]);
        if tag_created {
            let _ = super::cli::run(repo_path, ["reset", "--hard", &backup_tag]);
        }
        if auto_stashed {
            let _ = super::cli::run(repo_path, ["stash", "pop", "--quiet"]);
        }
        Ok(ApplyOutcome::Aborted {
            reason,
            backup_tag: backup_tag_opt.clone(),
        })
    };

    // ── 1. Working-tree replacements ─────────────────────────────────
    let mut wt_changed = 0usize;
    if !plan.apply_working_tree_paths.is_empty() {
        match apply_working_tree(repo_path, &plan) {
            Ok(n) => wt_changed = n,
            Err(e) => return rollback(format!("working-tree rewrite: {e:#}")),
        }
    }

    // Stage + commit the working-tree changes as ONE "chore(scrub)"
    // commit so the history stays clean. Only does this when we
    // actually changed something — a plan that's only commit-message
    // rewrites should not produce a spurious empty commit.
    if wt_changed > 0 {
        let add = super::cli::run(repo_path, ["add", "-A"]);
        if let Err(e) = add {
            return rollback(format!("git add -A: {e:#}"));
        }
        let commit_msg = format!(
            "chore(scrub): replace '{}' with '{}' across the working tree",
            plan.pattern, plan.replacement
        );
        let commit_result = super::cli::GitCommand::new(repo_path)
            .args(["commit", "-m", &commit_msg, "--no-verify"])
            .run_raw();
        match commit_result {
            Ok(out) if !out.status.success() => {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                return rollback(format!("commit working-tree scrub: {stderr}"));
            }
            Err(e) => return rollback(format!("commit working-tree scrub: {e:#}")),
            _ => {}
        }
    }

    // ── 2. Batch reword affected commit messages (oldest first) ──────
    let mut remap: Vec<(gix::ObjectId, gix::ObjectId)> = Vec::new();
    if !plan.apply_commit_oids.is_empty() {
        // We reword one commit at a time from oldest to newest.
        // Reasoning: a single-pass `git rebase -i` with a helper editor
        // would be faster but requires OS-level editor scripts that are
        // fragile cross-platform. Looping calls to `reword_commit` is
        // quadratic in the number of affected commits but linear in
        // practice for find-and-fix (N is typically < 20).
        //
        // Note: after each reword, the branch tip's OIDs shift. We
        // recompute the target oids by applying the remap to the
        // pending list as we go.
        let mut pending: Vec<gix::ObjectId> = sort_commits_oldest_first(
            repo_path,
            plan.apply_commit_oids.clone(),
        )
        .unwrap_or(plan.apply_commit_oids.clone());

        while let Some(next) = pending.first().copied() {
            // Fetch the current message for this commit — it may be
            // a RESHAPED version of the original if an earlier
            // rewrite in the loop touched an ancestor.
            let current_msg =
                match super::cli::run(repo_path, ["log", "-1", "--format=%B", &next.to_string()]) {
                    Ok(out) => out.stdout_str().trim_end_matches('\n').to_string(),
                    Err(e) => {
                        return rollback(format!(
                            "fetch message for {}: {e:#}",
                            short_oid(&next)
                        ));
                    }
                };
            let new_msg = current_msg.replace(&plan.pattern, &plan.replacement);
            if new_msg == current_msg {
                // No-op — the term vanished before we got to this
                // commit. Skip.
                pending.remove(0);
                continue;
            }
            match reword_commit(repo_path, next, &new_msg) {
                Ok(crate::git::RewordOutcome::Success {
                    new_target_oid, ..
                }) => {
                    remap.push((next, new_target_oid));
                    // Rebuild pending: replace any oid equal to the
                    // old `next` with `new_target_oid`. Downstream
                    // commits keep their oids because nothing
                    // ancestor-wise changed past this point (gix's
                    // OID is content-addressed; only the target and
                    // its ancestors' OIDs shift).
                    pending.remove(0);
                }
                Ok(crate::git::RewordOutcome::Aborted { reason, .. }) => {
                    return rollback(format!(
                        "reword of {} aborted: {reason}",
                        short_oid(&next)
                    ));
                }
                Err(e) => {
                    return rollback(format!(
                        "reword of {} failed: {e:#}",
                        short_oid(&next)
                    ));
                }
            }
        }
    }

    // ── 3. Restore the auto-stash ────────────────────────────────────
    if auto_stashed {
        let _ = super::cli::run(repo_path, ["stash", "pop", "--quiet"]);
    }

    Ok(ApplyOutcome::Success {
        working_tree_files_changed: wt_changed,
        commit_oid_remap: remap,
        backup_tag: backup_tag_opt,
        auto_stashed,
    })
}

/// Read each target file, do a literal `str::replace`, and write it
/// back. Returns the number of paths actually changed (files where
/// the replacement produced different bytes).
fn apply_working_tree(repo_path: &Path, plan: &ApplyPlan) -> Result<usize> {
    let mut changed = 0;
    for rel in &plan.apply_working_tree_paths {
        let absolute = repo_path.join(rel);
        let bytes = std::fs::read(&absolute)
            .with_context(|| format!("read {}", absolute.display()))?;
        // Use str::replace only when the file is UTF-8. Binaries are
        // excluded by the scan step (-I on git grep), so this check
        // is belt-and-braces — we'd rather skip a weird-encoded file
        // than corrupt it.
        let text = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let replaced = text.replace(&plan.pattern, &plan.replacement);
        if replaced != text {
            std::fs::write(&absolute, replaced)
                .with_context(|| format!("write {}", absolute.display()))?;
            changed += 1;
        }
    }
    Ok(changed)
}

fn sort_commits_oldest_first(
    repo_path: &Path,
    commits: Vec<gix::ObjectId>,
) -> Option<Vec<gix::ObjectId>> {
    if commits.len() <= 1 {
        return Some(commits);
    }
    let mut args: Vec<String> = vec![
        "rev-list".into(),
        "--topo-order".into(),
        "--no-walk".into(),
        "--reverse".into(),
    ];
    for c in &commits {
        args.push(c.to_string());
    }
    let out = super::cli::run(repo_path, args.iter().map(String::as_str)).ok()?;
    let mut sorted: Vec<gix::ObjectId> = Vec::with_capacity(commits.len());
    for line in out.stdout_str().lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(oid) = gix::ObjectId::from_hex(line.as_bytes()) {
            if commits.contains(&oid) {
                sorted.push(oid);
            }
        }
    }
    for oid in commits {
        if !sorted.contains(&oid) {
            sorted.push(oid);
        }
    }
    Some(sorted)
}

fn short_oid(oid: &gix::ObjectId) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_grep_z_output_extracts_path_line_and_match_range() {
        // Two records separated by \n. Each record: path\0lineno\0line.
        let data = b"src/foo.rs\x0012\x00    let hit = Fork-style;\nREADME\x005\x00# Fork-style";
        let out = parse_grep_z_output(data, "Fork-style");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].path, PathBuf::from("src/foo.rs"));
        assert_eq!(out[0].line_number, 12);
        assert_eq!(out[0].match_start, 14);
        assert_eq!(out[0].match_end, 14 + "Fork-style".len());
        assert_eq!(out[1].path, PathBuf::from("README"));
        assert_eq!(out[1].line_number, 5);
    }

    #[test]
    fn parse_grep_z_output_skips_malformed_records() {
        // Missing the \0 separator — should be ignored, not panic.
        let data = b"garbage line with no nuls";
        let out = parse_grep_z_output(data, "whatever");
        assert!(out.is_empty());
    }

    #[test]
    fn scan_with_empty_pattern_returns_nothing() {
        // Running `scan` with an empty pattern shouldn't touch git.
        let tmp = std::env::temp_dir();
        let result = scan(&tmp, "", true, true, 100).expect("scan");
        assert!(result.working_tree.is_empty());
        assert!(result.commit_messages.is_empty());
    }

    #[test]
    fn apply_with_no_selection_is_aborted() {
        let tmp = std::env::temp_dir();
        let plan = ApplyPlan {
            pattern: "x".into(),
            replacement: "y".into(),
            apply_working_tree_paths: Vec::new(),
            apply_commit_oids: Vec::new(),
        };
        match apply(&tmp, plan).expect("apply") {
            ApplyOutcome::Aborted { reason, .. } => {
                assert!(reason.contains("nothing selected"));
            }
            _ => panic!("expected Aborted"),
        }
    }
}
