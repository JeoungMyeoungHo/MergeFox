//! Operations on a set of non-linear commits ("commit basket").
//!
//! The UI lets the user Cmd/Ctrl-click to pick N commits anywhere in
//! the graph and then asks questions like "what did these commits
//! change in combination?" or "revert their effect on my working
//! tree". Those aren't expressible in stock `git` CLI without either
//! interactive rebase or a worktree-and-cherry-pick dance. This module
//! is the dance, centralised so the UI only sees typed inputs / outputs.
//!
//! Strategy
//! --------
//! For the combined-diff case:
//!   1. Sort the selection in topological order (oldest-first). Applying
//!      commits out of topo order regularly conflicts with itself.
//!   2. Pick a base commit. We use `git merge-base --octopus` over the
//!      selection; that's the latest commit every selected commit
//!      descends from, which minimises the patch stack we have to
//!      replay.
//!   3. Spin up a detached worktree at the base, cherry-pick --no-commit
//!      each selected commit in order.
//!   4. Compute `git diff --raw --patch` between the base tree and the
//!      resulting worktree — that's the "combined delta".
//!   5. Tear the worktree down unconditionally (Drop-based cleanup) so
//!      aborted runs don't leak `worktrees/` entries.
//!
//! Conflicts during step 3 abort with a `CombineError::Conflict` that
//! names the offending commit so the UI can surface which commit pair
//! clashed.
//!
//! The worktree lives under the OS temp dir and is isolated from the
//! user's main checkout — no stash / no HEAD motion on the real repo.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

use super::diff::{diff_for_commit_in, RepoDiff};

/// Why the combined-diff computation couldn't finish.
#[derive(Debug)]
pub enum CombineError {
    /// Fewer than two commits — not a meaningful "set" operation.
    NotEnoughCommits,
    /// `git merge-base` found no common ancestor among the selection.
    /// Usually means the user picked commits from two unrelated
    /// histories (e.g. subtree-merged subprojects).
    NoCommonAncestor,
    /// Cherry-pick mid-run hit a conflict that `--no-commit` couldn't
    /// auto-resolve. `blocking_commit` is the commit whose patch
    /// didn't apply cleanly on top of the prior replays.
    Conflict {
        blocking_commit: gix::ObjectId,
    },
    /// Any other git / IO failure, kept as a string for UI display.
    Other(String),
}

impl std::fmt::Display for CombineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotEnoughCommits => {
                write!(f, "pick at least two commits to build a combined diff")
            }
            Self::NoCommonAncestor => write!(
                f,
                "selected commits have no common ancestor — can't combine"
            ),
            Self::Conflict { blocking_commit } => write!(
                f,
                "commit {} conflicts when applied on top of the prior selection",
                short_oid(blocking_commit)
            ),
            Self::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for CombineError {}

/// What the UI gets back on success — the cumulative delta, plus the
/// topologically-sorted commits that were applied (so the view can
/// show a banner like "Combined diff of abc1234 ← def5678 ← ghi9abc").
#[derive(Debug, Clone)]
pub struct CombinedDiff {
    pub diff: RepoDiff,
    pub applied_order: Vec<gix::ObjectId>,
    pub base: gix::ObjectId,
}

/// Process-wide counter for worktree names — keeps concurrent calls
/// (e.g. two tabs triggering at once) from colliding on paths.
static WORKTREE_NONCE: AtomicU64 = AtomicU64::new(0);

/// Compute the combined diff of `commits` applied in topological
/// order on top of their most-recent common ancestor.
pub fn compute_combined_diff(
    repo_path: &Path,
    commits: &[gix::ObjectId],
) -> std::result::Result<CombinedDiff, CombineError> {
    if commits.len() < 2 {
        return Err(CombineError::NotEnoughCommits);
    }

    let base = match merge_base_octopus(repo_path, commits) {
        Ok(Some(oid)) => oid,
        Ok(None) => return Err(CombineError::NoCommonAncestor),
        Err(e) => return Err(CombineError::Other(e.to_string())),
    };

    let sorted = topo_sort(repo_path, commits).map_err(|e| CombineError::Other(e.to_string()))?;

    let wt = ScratchWorktree::create(repo_path, &base)
        .map_err(|e| CombineError::Other(e.to_string()))?;

    for oid in &sorted {
        match cherry_pick_no_commit(&wt.path, oid) {
            Ok(()) => {}
            Err(CherryPickError::Conflict) => {
                return Err(CombineError::Conflict {
                    blocking_commit: *oid,
                });
            }
            Err(CherryPickError::Other(e)) => {
                return Err(CombineError::Other(e));
            }
        }
    }

    // Snapshot the WT tree and diff it against the base tree. We do
    // this by committing the intermediate state to a detached commit
    // — that gives us a stable OID we can feed to the existing diff
    // machinery without duplicating patch-parsing code here.
    let tip_oid = commit_worktree_state(&wt.path, &base)
        .map_err(|e| CombineError::Other(e.to_string()))?;

    let diff = diff_for_commit_in(repo_path, tip_oid, Some(base))
        .map_err(|e| CombineError::Other(e.to_string()))?;

    Ok(CombinedDiff {
        diff,
        applied_order: sorted,
        base,
    })
}

// ---------------- helpers ----------------

fn merge_base_octopus(repo_path: &Path, commits: &[gix::ObjectId]) -> Result<Option<gix::ObjectId>> {
    // `merge-base --octopus a b c` gives the best ancestor for all of
    // them in one call. Fails (non-zero) if there's no shared ancestor.
    let mut args = vec!["merge-base".to_string(), "--octopus".to_string()];
    for c in commits {
        args.push(c.to_string());
    }
    let output = super::cli::GitCommand::new(repo_path)
        .args(args.iter().map(String::as_str))
        .run_raw()
        .context("git merge-base")?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let first = match text.lines().next() {
        Some(s) => s.trim(),
        None => return Ok(None),
    };
    if first.is_empty() {
        return Ok(None);
    }
    let oid = gix::ObjectId::from_hex(first.as_bytes()).context("parse merge-base oid")?;
    Ok(Some(oid))
}

fn topo_sort(repo_path: &Path, commits: &[gix::ObjectId]) -> Result<Vec<gix::ObjectId>> {
    // `rev-list --topo-order --no-walk --reverse a b c` prints the
    // input commits in topological order (ancestors before
    // descendants) — exactly the apply order cherry-pick wants.
    let mut args = vec![
        "rev-list".to_string(),
        "--topo-order".to_string(),
        "--no-walk".to_string(),
        "--reverse".to_string(),
    ];
    for c in commits {
        args.push(c.to_string());
    }
    let output = super::cli::run(repo_path, args.iter().map(String::as_str))
        .context("git rev-list --topo-order")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let oid = gix::ObjectId::from_hex(line.as_bytes()).context("parse topo-sorted oid")?;
        out.push(oid);
    }
    Ok(out)
}

struct ScratchWorktree {
    repo_path: PathBuf,
    path: PathBuf,
}

impl ScratchWorktree {
    fn create(repo_path: &Path, base: &gix::ObjectId) -> Result<Self> {
        let nonce = WORKTREE_NONCE.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let name = format!("mergefox-basket-{pid}-{nonce}");
        let dir = std::env::temp_dir().join(name);
        super::cli::run(
            repo_path,
            [
                "worktree",
                "add",
                "--detach",
                "--no-checkout",
                "-f",
                dir.to_str().context("tempdir utf-8")?,
                &base.to_string(),
            ],
        )
        .with_context(|| format!("git worktree add {}", dir.display()))?;
        // We passed --no-checkout so the worktree is empty; force the
        // checkout now so subsequent cherry-picks have a working tree
        // to apply into. Doing checkout as a second step avoids a
        // macOS bug where "worktree add" with --checkout sometimes
        // leaves index state from a prior HEAD on fast filesystems.
        super::cli::run(&dir, ["checkout", "--detach", &base.to_string()])
            .context("checkout base in scratch worktree")?;
        Ok(Self {
            repo_path: repo_path.to_path_buf(),
            path: dir,
        })
    }
}

impl Drop for ScratchWorktree {
    fn drop(&mut self) {
        // Best-effort cleanup; log-only on failure because the Drop
        // path can't propagate errors. `worktree remove --force`
        // handles the common "uncommitted state" case.
        if let Err(e) = super::cli::run(
            &self.repo_path,
            [
                "worktree",
                "remove",
                "--force",
                self.path.to_str().unwrap_or(""),
            ],
        ) {
            tracing::warn!(
                error = %format!("{e:#}"),
                path = %self.path.display(),
                "scratch worktree cleanup failed"
            );
            // Fall back to a manual rmdir so we don't leak the
            // directory even if git forgot about it.
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

enum CherryPickError {
    Conflict,
    Other(String),
}

fn cherry_pick_no_commit(
    worktree: &Path,
    oid: &gix::ObjectId,
) -> std::result::Result<(), CherryPickError> {
    // `-n` = no commit; `--allow-empty` keeps trivially-empty commits
    // (rebase artefacts) from aborting the chain; `-X ours` is NOT
    // used because we want honest conflicts surfaced to the user.
    let result = super::cli::GitCommand::new(worktree)
        .args([
            "cherry-pick",
            "--no-commit",
            "--allow-empty",
            &oid.to_string(),
        ])
        .run_raw();
    match result {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            // On conflict git exits 1 and leaves the working tree
            // with markers. Reset it so the next invocation of this
            // helper (aborted higher up) doesn't inherit partial
            // state, then report.
            let _ = super::cli::run(worktree, ["cherry-pick", "--abort"]);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let lower = stderr.to_ascii_lowercase();
            if lower.contains("conflict") || lower.contains("could not apply") {
                Err(CherryPickError::Conflict)
            } else {
                Err(CherryPickError::Other(stderr.trim().to_string()))
            }
        }
        Err(e) => Err(CherryPickError::Other(format!("{e:#}"))),
    }
}

/// After replaying cherry-picks, snapshot the worktree as a commit
/// so the caller can diff against it with existing machinery.
fn commit_worktree_state(worktree: &Path, base: &gix::ObjectId) -> Result<gix::ObjectId> {
    // Stage everything — cherry-pick --no-commit already staged its
    // own deltas, but `add -A` picks up any stragglers and is cheap.
    super::cli::run(worktree, ["add", "-A"]).context("git add -A in scratch worktree")?;

    // Allow empty so a no-op combined diff (every cherry-pick added
    // and reverted something) still produces a commit; otherwise
    // commit-tree would choke on an empty index.
    let output = super::cli::run(
        worktree,
        [
            "commit",
            "-m",
            "mergefox combined-diff snapshot",
            "--allow-empty",
            "--no-verify",
        ],
    )
    .context("commit scratch snapshot")?;
    let _ = output;

    // Fetch the freshly created HEAD.
    let head = super::cli::run(worktree, ["rev-parse", "HEAD"]).context("rev-parse HEAD")?;
    let text = head.stdout_str();
    let first = text.lines().next().unwrap_or("").trim();
    gix::ObjectId::from_hex(first.as_bytes())
        .with_context(|| format!("parse scratch-commit oid from '{first}'; base={}", base))
}

fn short_oid(oid: &gix::ObjectId) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}

/// Return a clone of `diff` with `files` restricted to those whose
/// display path equals `focus_path`.
///
/// WHY a pure helper on top of `RepoDiff` rather than filtering inside
/// `compute_combined_diff`: the UI needs to toggle between "show all
/// files the basket touched" and "show only one of them" without
/// recomputing the cherry-pick chain — that chain takes seconds on
/// real-world selections (it spawns a detached worktree and runs N
/// cherry-picks). We keep the full diff around in the workspace state
/// and re-derive the focused view on the fly.
///
/// Matching semantics: we compare against `FileDiff::display_path`
/// (new_path where present, otherwise old_path). That matches what the
/// UI's file list and the picker candidate list both render, so the
/// round-trip "pick this row → get this row filtered" is predictable.
/// Renamed files are handled naturally because display_path tracks the
/// new name.
pub fn filter_combined_diff_to_path(diff: &RepoDiff, focus_path: &str) -> RepoDiff {
    let kept: Vec<_> = diff
        .files
        .iter()
        .filter(|f| f.display_path() == focus_path)
        .cloned()
        .collect();
    RepoDiff {
        title: diff.title.clone(),
        commit_message: diff.commit_message.clone(),
        commit_author: diff.commit_author.clone(),
        commit_author_email: diff.commit_author_email.clone(),
        commit_author_time: diff.commit_author_time,
        commit_oid: diff.commit_oid,
        commit_parent_oids: diff.commit_parent_oids.clone(),
        files: kept.into_boxed_slice(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::{DeltaStatus, FileDiff, FileKind, Hunk, RepoDiff};
    use std::path::PathBuf;

    fn mk_file(path: &str) -> FileDiff {
        FileDiff {
            old_path: Some(PathBuf::from(path)),
            new_path: Some(PathBuf::from(path)),
            status: DeltaStatus::Modified,
            kind: FileKind::Text {
                hunks: Vec::<Hunk>::new(),
                lines_added: 0,
                lines_removed: 0,
                truncated: false,
            },
            old_size: 0,
            new_size: 0,
            old_oid: None,
            new_oid: None,
        }
    }

    fn mk_diff(paths: &[&str]) -> RepoDiff {
        RepoDiff {
            title: "t".into(),
            commit_message: None,
            commit_author: None,
            commit_author_email: None,
            commit_author_time: None,
            commit_oid: None,
            commit_parent_oids: Vec::new(),
            files: paths.iter().map(|p| mk_file(p)).collect::<Vec<_>>().into_boxed_slice(),
        }
    }

    #[test]
    fn filter_keeps_matching_path() {
        let diff = mk_diff(&["src/a.rs", "src/b.rs", "src/c.rs"]);
        let focused = filter_combined_diff_to_path(&diff, "src/b.rs");
        assert_eq!(focused.files.len(), 1);
        assert_eq!(focused.files[0].display_path(), "src/b.rs");
    }

    #[test]
    fn filter_preserves_commit_metadata() {
        let mut diff = mk_diff(&["x.rs", "y.rs"]);
        diff.title = "synthetic".into();
        diff.commit_author = Some("tester".into());
        let focused = filter_combined_diff_to_path(&diff, "x.rs");
        assert_eq!(focused.title, "synthetic");
        assert_eq!(focused.commit_author.as_deref(), Some("tester"));
    }

    #[test]
    fn filter_empty_when_no_match() {
        let diff = mk_diff(&["src/a.rs"]);
        let focused = filter_combined_diff_to_path(&diff, "missing.rs");
        assert!(focused.files.is_empty());
    }
}
