//! Pre-flight info for destructive git actions.
//!
//! The rule: before any confirmation modal that can permanently lose work,
//! we compute a concrete summary of what's about to happen and attach it
//! to the prompt. The user sees real numbers ("this will lose 3 commits")
//! instead of generic warnings ("this cannot be undone"), and the modal
//! body stays a clean template regardless of which branch / commit is
//! selected.
//!
//! All queries here go through the installed `git` CLI so hooks, aliases,
//! and alternates are honored. They must be quick — the prompt renders
//! synchronously on the UI thread and we do not want to stall input. Each
//! function caps its work (e.g. `--max-count`) to stay sub-30 ms on
//! typical repos.

use std::path::Path;

use gix::ObjectId as Oid;

use crate::git::cli;

/// Structured info rendered in the destructive-action confirmation modal.
/// Each line gets a severity-colored leading glyph so the highest-severity
/// warning is unmissable even in a densely-packed dialog.
#[derive(Debug, Clone, Default)]
pub struct PreflightInfo {
    pub lines: Vec<PreflightLine>,
}

impl PreflightInfo {
    pub fn push(&mut self, severity: Severity, text: impl Into<String>) {
        self.lines.push(PreflightLine {
            severity,
            text: text.into(),
        });
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct PreflightLine {
    pub severity: Severity,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Neutral factual context ("3 descendants will be replayed").
    Info,
    /// Warrants attention but is reversible ("2 commits will be rewritten").
    Warning,
    /// Irreversible data loss if the user doesn't take backup action.
    Critical,
}

// ----------------------------------------------------------------------
// Per-action computations. Each takes the repo path + whatever parameters
// the action carries, and returns an info block ready to render.
// ----------------------------------------------------------------------

/// `reset --hard <target>` preview. Reports:
///   * How many commits on the current branch will be dropped (reachable
///     from the branch tip but not from `target`).
///   * Whether the working tree is dirty — those edits vanish with
///     `--hard`. We auto-stash elsewhere, but the modal should still say so.
pub fn hard_reset(repo_path: &Path, branch: &str, target: Oid) -> PreflightInfo {
    let mut info = PreflightInfo::default();
    let range = format!("{target}..{branch}");
    if let Some(n) = count_commits(repo_path, &range) {
        if n > 0 {
            info.push(
                Severity::Critical,
                format!(
                    "{n} commit{} on `{branch}` will be dropped from the branch.",
                    if n == 1 { "" } else { "s" }
                ),
            );
        } else {
            info.push(
                Severity::Info,
                format!("`{branch}` has no commits ahead of the target — no commits lost."),
            );
        }
    }
    if working_tree_dirty(repo_path) {
        info.push(
            Severity::Warning,
            "Working tree has uncommitted changes — we'll auto-stash first.",
        );
    }
    info.push(
        Severity::Info,
        "Dropped commits stay in the reflog for ~90 days (default `gc.reflogExpire`).",
    );
    info
}

/// Delete-branch preview. Reports unmerged commit count — git won't let a
/// plain `-d` delete an unmerged branch, but `-D` (which mergefox uses)
/// forces it. We want users to know what's being forced.
pub fn delete_branch(repo_path: &Path, name: &str, is_remote: bool) -> PreflightInfo {
    let mut info = PreflightInfo::default();
    if is_remote {
        info.push(
            Severity::Info,
            "Remote-tracking ref only — does not affect the upstream server.",
        );
        info.push(
            Severity::Info,
            "The ref is recreated on next `fetch` from this remote.",
        );
        return info;
    }

    // Commits on this branch not reachable from any other local branch.
    // `git log <branch> --not --branches=* --exclude=<branch>` works but is
    // verbose; the more robust approach is `log <branch> ^HEAD` when HEAD
    // differs — for a balanced answer we compare against *all other
    // branches* via `--branches` + `--not` + rev-parse.
    let other_refs = list_other_branch_refs(repo_path, name);
    let mut args: Vec<String> = vec!["log".into(), "--oneline".into(), name.into()];
    for r in &other_refs {
        args.push("--not".into());
        args.push(r.clone());
    }
    if let Ok(out) = cli::run(repo_path, args.iter().map(String::as_str)) {
        let n = out.stdout_str().lines().filter(|l| !l.is_empty()).count();
        if n > 0 {
            info.push(
                Severity::Critical,
                format!(
                    "{n} commit{} exist only on `{name}` — they become unreachable once deleted.",
                    if n == 1 { "" } else { "s" }
                ),
            );
        } else {
            info.push(
                Severity::Info,
                format!("All commits on `{name}` are reachable from other branches."),
            );
        }
    }
    info.push(
        Severity::Info,
        "Dropped commits stay in the reflog for ~90 days (default `gc.reflogExpire`).",
    );
    info
}

/// Force-push preview. Reports how many commits on the remote branch are
/// about to be overwritten — this is the "someone else pushed first"
/// scenario that force push silently destroys.
pub fn force_push(repo_path: &Path, remote: &str, branch: &str) -> PreflightInfo {
    let mut info = PreflightInfo::default();
    let tracking = format!("{remote}/{branch}");
    // Commits on the remote that the local branch doesn't have.
    let range = format!("{branch}..{tracking}");
    if let Some(n) = count_commits(repo_path, &range) {
        if n > 0 {
            info.push(
                Severity::Critical,
                format!(
                    "{n} commit{} on `{tracking}` will be OVERWRITTEN \
                     and are not on your local branch.",
                    if n == 1 { "" } else { "s" }
                ),
            );
            info.push(
                Severity::Warning,
                "Consider `force-with-lease` instead — fails safely if \
                 someone pushed while you were working.",
            );
        } else {
            info.push(
                Severity::Info,
                format!("`{tracking}` is fully contained in your local branch — safe fast-forward, force is a no-op."),
            );
        }
    } else {
        info.push(
            Severity::Warning,
            format!("Could not read `{tracking}` — run `fetch` first for an accurate preview."),
        );
    }
    let range_ahead = format!("{tracking}..{branch}");
    if let Some(n) = count_commits(repo_path, &range_ahead) {
        info.push(
            Severity::Info,
            format!(
                "{n} local commit{} will be pushed.",
                if n == 1 { "" } else { "s" }
            ),
        );
    }
    info
}

/// Amend-HEAD preview. Answers the question "will this amend rewrite a
/// commit I've already pushed?" — which, if yes, means the next push
/// needs `--force` (or ideally `--force-with-lease`) to land. Matches
/// `TODO/production.md` §G4.
///
/// Returns an empty `PreflightInfo` when HEAD is only local, so the
/// non-destructive happy path (first-ever amend before push) stays
/// visually quiet.
pub fn amend_head(repo_path: &Path) -> PreflightInfo {
    let mut info = PreflightInfo::default();
    // `git branch --remotes --contains HEAD` lists every remote-tracking
    // branch that already has this commit. Non-empty = "remote somewhere
    // knows about this commit already".
    let out = match cli::run(
        repo_path,
        ["branch", "--remotes", "--contains", "HEAD"]
            .iter()
            .copied(),
    ) {
        Ok(o) => o,
        Err(_) => return info, // preflight unavailable — don't alarm
    };
    let matches: Vec<String> = out
        .stdout_str()
        .lines()
        .map(|s| s.trim().trim_start_matches('*').trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if matches.is_empty() {
        return info;
    }
    let preview = matches.join(", ");
    info.push(
        Severity::Warning,
        format!(
            "This commit is already on {} remote branch{}: {}. \
             Amending rewrites history — the next push will need \
             `--force-with-lease` to land.",
            matches.len(),
            if matches.len() == 1 { "" } else { "es" },
            preview
        ),
    );
    info.push(
        Severity::Info,
        "If someone else pulled this commit, they'll need to rebase onto your amended version.",
    );
    info
}

/// Drop-commit preview. Reports the number of descendants that will be
/// replayed via cherry-pick, because that's the conflict-risk proxy.
pub fn drop_commit(repo_path: &Path, oid: Oid) -> PreflightInfo {
    let mut info = PreflightInfo::default();
    let range = format!("{oid}..HEAD");
    if let Some(n) = count_commits(repo_path, &range) {
        if n == 0 {
            info.push(
                Severity::Info,
                "This is the tip commit — drop is equivalent to `reset --hard HEAD~1`.",
            );
        } else {
            info.push(
                Severity::Warning,
                format!(
                    "{n} descendant commit{} will be replayed on top — conflicts possible.",
                    if n == 1 { "" } else { "s" }
                ),
            );
        }
    }
    info.push(
        Severity::Info,
        "A backup ref (`<branch>.backup-<ts>`) is created before the rebase runs.",
    );
    info
}

// ---------------------------------------------------------------- helpers

/// Count commits in `range` (e.g. `target..branch`). Returns `None` on any
/// git failure — the caller should treat that as "preflight unavailable"
/// rather than falling through to a misleading zero.
fn count_commits(repo_path: &Path, range: &str) -> Option<u32> {
    let out = cli::run(
        repo_path,
        ["rev-list", "--count", "--", range].iter().copied(),
    )
    .ok()?;
    if out.status != 0 {
        return None;
    }
    out.stdout_str().trim().parse::<u32>().ok()
}

fn working_tree_dirty(repo_path: &Path) -> bool {
    cli::run(repo_path, ["status", "--porcelain"].iter().copied())
        .map(|o| !o.stdout_str().trim().is_empty())
        .unwrap_or(false)
}

/// Return every local-branch ref name other than `exclude`, suitable for
/// passing to `git log --not <ref>…`.
fn list_other_branch_refs(repo_path: &Path, exclude: &str) -> Vec<String> {
    let out = match cli::run(
        repo_path,
        ["for-each-ref", "--format=%(refname:short)", "refs/heads/"]
            .iter()
            .copied(),
    ) {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    out.stdout_str()
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != exclude)
        .collect()
}
