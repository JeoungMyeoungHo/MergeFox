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
use super::repo::{auto_stash_path, AutoStashOpts, AutoStashOutcome};

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

// ============================================================
// Phase 5: Revert-to-working-tree over a basket of commits
// ============================================================

/// Outcome of a basket `git revert --no-commit <oids…>` chain, from the
/// caller's (UI thread's) point of view.
///
/// The distinction between `Clean` and `Conflicts` is NOT "did git exit
/// zero" — git's revert machinery can partially succeed and then stop on
/// a conflict, leaving `REVERT_HEAD` in place. We classify based on
/// conflict markers in the working tree after the command returns. The
/// `Aborted` variant is only produced when we actively ran
/// `git revert --abort` to roll back, and corresponds to an unclean
/// state we couldn't reason about (e.g. git exited non-zero with no
/// conflicted paths — usually a permissions / disk-full scenario).
#[derive(Debug, Clone)]
pub enum RevertOutcome {
    /// All commits were reverted into the working tree cleanly.
    Clean {
        commits_reverted: usize,
        auto_stashed: bool,
    },
    /// Git stopped partway with conflict markers in the working tree.
    /// `REVERT_HEAD` is still in place — the caller must surface the
    /// conflicts modal and let the user `Continue` / `Abort` through
    /// the normal resolver flow.
    Conflicts {
        commits_reverted: usize,
        conflicted_paths: Vec<PathBuf>,
        auto_stashed: bool,
    },
    /// Something went wrong that we couldn't route to the conflicts
    /// modal. We ran `git revert --abort` to restore the pre-op tree.
    Aborted { reason: String },
}

/// Run `git revert --no-commit <oid1> <oid2> …` over the basket.
///
/// Synchronous; intended to be called from a worker thread. Flow:
///   1. Probe the working tree for dirtiness; auto-stash if needed.
///   2. Sort commits newest-first (reverse topological).
///   3. `git revert --no-commit --no-edit` over the whole list.
///   4. Classify outcome: Clean / Conflicts / Aborted.
pub fn revert_to_working_tree(
    repo_path: &Path,
    commits: &[gix::ObjectId],
) -> Result<RevertOutcome> {
    if commits.is_empty() {
        return Ok(RevertOutcome::Clean {
            commits_reverted: 0,
            auto_stashed: false,
        });
    }

    let auto_stashed = match auto_stash_path(
        repo_path,
        "basket revert",
        AutoStashOpts::default(),
    )
    .context("auto-stash before basket revert")?
    {
        AutoStashOutcome::Clean => false,
        AutoStashOutcome::Stashed { .. } => true,
        AutoStashOutcome::Refused { reason } => {
            return Ok(RevertOutcome::Aborted {
                reason: reason.to_string(),
            });
        }
    };

    let ordered = sort_reverse_topo(repo_path, commits)?;

    let mut args: Vec<String> = vec![
        "revert".to_owned(),
        "--no-commit".to_owned(),
        "--no-edit".to_owned(),
    ];
    args.extend(ordered.iter().map(|o| o.to_string()));

    let output = super::cli::GitCommand::new(repo_path)
        .args(&args)
        .run_raw()
        .context("spawn git revert")?;

    if output.status.success() {
        return Ok(RevertOutcome::Clean {
            commits_reverted: commits.len(),
            auto_stashed,
        });
    }

    let conflicts = unmerged_paths(repo_path).unwrap_or_default();
    if !conflicts.is_empty() {
        let remaining = sequencer_remaining_count(repo_path).unwrap_or(0);
        let commits_reverted = commits.len().saturating_sub(remaining + 1);
        return Ok(RevertOutcome::Conflicts {
            commits_reverted,
            conflicted_paths: conflicts,
            auto_stashed,
        });
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let _ = super::cli::run(repo_path, ["revert", "--abort"]);
    Ok(RevertOutcome::Aborted {
        reason: if stderr.is_empty() {
            format!("git revert exited with status {}", output.status)
        } else {
            stderr
        },
    })
}

/// Return the basket commits in reverse topological (newest-first) order.
fn sort_reverse_topo(
    repo_path: &Path,
    commits: &[gix::ObjectId],
) -> Result<Vec<gix::ObjectId>> {
    if commits.len() <= 1 {
        return Ok(commits.to_vec());
    }
    let mut args: Vec<String> = vec![
        "rev-list".to_owned(),
        "--topo-order".to_owned(),
        "--no-walk".to_owned(),
    ];
    args.extend(commits.iter().map(|o| o.to_string()));
    let out = match super::cli::GitCommand::new(repo_path).args(&args).run() {
        Ok(out) => out,
        Err(_) => return Ok(commits.to_vec()),
    };
    let mut sorted = Vec::with_capacity(commits.len());
    for line in out.stdout_str().lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(oid) = gix::ObjectId::from_hex(trimmed.as_bytes()) {
            if commits.contains(&oid) {
                sorted.push(oid);
            }
        }
    }
    for oid in commits {
        if !sorted.contains(oid) {
            sorted.push(*oid);
        }
    }
    Ok(sorted)
}

fn unmerged_paths(repo_path: &Path) -> Result<Vec<PathBuf>> {
    let out = super::cli::run(
        repo_path,
        ["status", "--porcelain=v1", "-z", "--untracked-files=no"],
    )?;
    Ok(parse_unmerged_z(&out.stdout))
}

/// Pure parser for `git status --porcelain=v1 -z` output, returning the
/// paths git classifies as unmerged.
fn parse_unmerged_z(data: &[u8]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        if pos + 3 > data.len() {
            break;
        }
        let x = data[pos] as char;
        let y = data[pos + 1] as char;
        pos += 3;

        let path_start = pos;
        while pos < data.len() && data[pos] != 0 {
            pos += 1;
        }
        if pos >= data.len() {
            break;
        }
        let path = PathBuf::from(String::from_utf8_lossy(&data[path_start..pos]).as_ref());
        pos += 1;

        if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
            while pos < data.len() && data[pos] != 0 {
                pos += 1;
            }
            if pos < data.len() {
                pos += 1;
            }
        }

        let conflicted = matches!(
            (x, y),
            ('U', 'U')
                | ('A', 'A')
                | ('D', 'D')
                | ('D', 'U')
                | ('U', 'D')
                | ('A', 'U')
                | ('U', 'A')
        );
        if conflicted {
            out.push(path);
        }
    }
    out
}

// ============================================================
// Phase 6: Non-linear squash (rewrite history)
// ============================================================

/// Outcome of a basket squash, from the caller's (UI thread's) point of view.
///
/// This is deliberately binary — `Success` or `Aborted`. There's no
/// "Conflicts" variant (unlike `RevertOutcome`) because a squash that
/// hits a conflict mid-rebuild is rolled back *inside* the worker: we
/// reset to the backup tag, restore the auto-stash, and report the
/// failure. WHY: mid-squash conflict resolution would leave the user in
/// detached-HEAD-with-CHERRY-PICK-HEAD state, which is extremely hard to
/// reason about for a v1 feature. The backup tag is our insurance — we
/// can always get back to it, and we always do on any failure.
#[derive(Debug, Clone)]
pub enum SquashOutcome {
    /// Squash and rebase completed cleanly. `new_head_oid` is the tip of
    /// the rewritten branch; `backup_tag` is the ref name of the safety
    /// tag we created at the old HEAD (preserved for undo / reflog UI).
    Success {
        new_head_oid: gix::ObjectId,
        backup_tag: String,
    },
    /// Something went wrong; we rolled back to pre-squash state. If
    /// `backup_tag_created` is `Some`, the tag is still on disk — the
    /// caller can surface it as an "Undo" affordance or just let it
    /// sit (tags are cheap, and retaining them keeps the reflog trail
    /// discoverable). `reason` is human-readable for toast display.
    Aborted {
        reason: String,
        backup_tag_created: Option<String>,
    },
}

/// Squash the commits in `commits` into a single new commit with
/// `new_message` as its message, then rebase the rest of the branch on
/// top. Rewrites history in-place on the current branch.
///
/// Flow:
///   1. Validate: ≥2 commits, no merge commits, no HEAD, all commits
///      must be ancestors of HEAD.
///   2. Auto-stash the working tree (reuse auto_stash_path).
///   3. Create a backup tag `mergefox/basket-squash/<ISO-timestamp>` at
///      current HEAD. This is the rollback anchor.
///   4. Find the octopus merge-base of basket commits (or basket ∪ HEAD).
///   5. Capture the current branch ref (so we can move it at the end).
///   6. Detach HEAD at merge-base, cherry-pick each basket commit in
///      topo order with --no-commit, then commit ONCE with `new_message`.
///   7. Compute the "keep" list: commits between merge-base and old HEAD
///      that are NOT in the basket, in topo order. Cherry-pick each on
///      top of the squashed commit.
///   8. Move the branch ref to the new HEAD. Restore stash.
///
/// Any failure after step 3 triggers rollback: reset to the backup tag,
/// restore auto-stash, return Aborted with the failure reason.
///
/// WHY in-place on the real worktree, not a ScratchWorktree: the goal
/// is to REWRITE the branch the user is on. A scratch worktree could
/// produce the new tip, but then we'd have to move the real branch ref
/// across worktrees which git disallows when the branch is checked out
/// anywhere. Using the real worktree with a backup tag as insurance is
/// simpler and gives the user an immediate visual result. The backup
/// tag is the safety net.
pub fn squash_basket_into_one(
    repo_path: &Path,
    commits: &[gix::ObjectId],
    new_message: &str,
) -> SquashOutcome {
    // ---- 1. Pre-flight validation ----
    if commits.len() < 2 {
        return SquashOutcome::Aborted {
            reason: "Need at least two commits to squash.".to_string(),
            backup_tag_created: None,
        };
    }
    if new_message.trim().is_empty() {
        return SquashOutcome::Aborted {
            reason: "Commit message cannot be empty.".to_string(),
            backup_tag_created: None,
        };
    }

    let head_oid = match super::cli::run_line(repo_path, ["rev-parse", "HEAD"]) {
        Ok(s) if !s.is_empty() => match gix::ObjectId::from_hex(s.trim().as_bytes()) {
            Ok(oid) => oid,
            Err(e) => {
                return SquashOutcome::Aborted {
                    reason: format!("parse HEAD oid: {e}"),
                    backup_tag_created: None,
                }
            }
        },
        Ok(_) => {
            return SquashOutcome::Aborted {
                reason: "HEAD is empty (no commits yet).".to_string(),
                backup_tag_created: None,
            }
        }
        Err(e) => {
            return SquashOutcome::Aborted {
                reason: format!("rev-parse HEAD: {e:#}"),
                backup_tag_created: None,
            }
        }
    };

    if commits.iter().any(|c| *c == head_oid) {
        return SquashOutcome::Aborted {
            reason: "Basket must not contain HEAD — squashing HEAD into itself makes no sense. \
                     Deselect the tip commit and try again."
                .to_string(),
            backup_tag_created: None,
        };
    }

    // Reject merge commits — cherry-pick of a merge needs `-m` with a
    // parent index we can't infer from a basket selection, and the
    // "combine two histories" semantics is not what the user expects
    // when they ticked "squash these N commits".
    if let Err(msg) = reject_merge_commits(repo_path, commits) {
        return SquashOutcome::Aborted {
            reason: msg,
            backup_tag_created: None,
        };
    }

    // Every commit in the basket must be reachable from HEAD. If not,
    // we'd have to reparent unrelated history, and there's no sane
    // definition of "the rest of the branch" to rebase.
    if let Err(msg) = ensure_all_ancestors_of(repo_path, commits, &head_oid) {
        return SquashOutcome::Aborted {
            reason: msg,
            backup_tag_created: None,
        };
    }

    // Figure out the branch (if any) we need to move at the end. If the
    // user is detached, we still rewrite, but there's no ref to move.
    let branch_name = current_branch(repo_path);

    // ---- 2. Auto-stash ----
    let stashed = match auto_stash_path(repo_path, "basket squash", AutoStashOpts::default()) {
        Ok(AutoStashOutcome::Clean) => false,
        Ok(AutoStashOutcome::Stashed { .. }) => true,
        Ok(AutoStashOutcome::Refused { reason }) => {
            return SquashOutcome::Aborted {
                reason: reason.to_string(),
                backup_tag_created: None,
            };
        }
        Err(e) => {
            return SquashOutcome::Aborted {
                reason: format!("auto-stash before squash: {e:#}"),
                backup_tag_created: None,
            };
        }
    };

    // ---- 3. Backup tag ----
    let backup_tag = backup_tag_name(now_unix_seconds());
    if let Err(e) = super::cli::run(
        repo_path,
        ["tag", backup_tag.as_str(), &head_oid.to_string()],
    ) {
        // If the tag creation fails we have NOT made any destructive
        // change yet; restore stash and bail.
        let _ = maybe_pop_stash(repo_path, stashed);
        return SquashOutcome::Aborted {
            reason: format!("create backup tag: {e:#}"),
            backup_tag_created: None,
        };
    }

    // Helper closure: any failure below this point invokes rollback.
    let rollback = |reason: String| -> SquashOutcome {
        // Reset working dir to the backup tag. `checkout -f` first to
        // ensure we're on a sane ref before `reset --hard` blows away
        // any in-flight cherry-pick state. We always try both; either
        // can fail harmlessly.
        let _ = super::cli::run(repo_path, ["cherry-pick", "--abort"]);
        if let Some(ref b) = branch_name {
            let _ = super::cli::run(repo_path, ["checkout", "-f", b]);
            let _ = super::cli::run(repo_path, ["reset", "--hard", backup_tag.as_str()]);
        } else {
            let _ = super::cli::run(repo_path, ["checkout", "-f", backup_tag.as_str()]);
        }
        let _ = maybe_pop_stash(repo_path, stashed);
        SquashOutcome::Aborted {
            reason,
            backup_tag_created: Some(backup_tag.clone()),
        }
    };

    // ---- 4. Merge-base ----
    // Include HEAD in the octopus so we pick the ancestor relevant to
    // "from here, rebuild up to HEAD". Without HEAD in the set, two
    // commits on the same linear chain give a merge-base equal to the
    // oldest — which is fine — but we want the guaranteed-ancestor-of-
    // HEAD interpretation for the rebase step anyway.
    let mut mb_inputs = commits.to_vec();
    mb_inputs.push(head_oid);
    let base = match merge_base_octopus(repo_path, &mb_inputs) {
        Ok(Some(oid)) => oid,
        Ok(None) => {
            return rollback(
                "Selected commits share no common ancestor with HEAD — \
                 refusing to rebuild an orphan history."
                    .to_string(),
            );
        }
        Err(e) => {
            return rollback(format!("merge-base: {e:#}"));
        }
    };

    // ---- 5. Compute "keep" list: commits between base..HEAD minus basket ----
    let keep_ordered = match commits_between_excluding(repo_path, &base, &head_oid, commits) {
        Ok(v) => v,
        Err(e) => return rollback(format!("enumerate branch commits: {e:#}")),
    };

    // ---- 6. Detach at merge-base and apply squash ----
    if let Err(e) = super::cli::run(
        repo_path,
        ["checkout", "--detach", &base.to_string()],
    ) {
        return rollback(format!("checkout --detach {base}: {e:#}"));
    }

    // Topo-sort basket commits oldest-first so patches stack predictably.
    let basket_sorted = match topo_sort(repo_path, commits) {
        Ok(v) => v,
        Err(e) => return rollback(format!("topo-sort basket: {e:#}")),
    };

    for oid in &basket_sorted {
        match cherry_pick_no_commit(repo_path, oid) {
            Ok(()) => {}
            Err(CherryPickError::Conflict) => {
                return rollback(format!(
                    "Cherry-pick of {} conflicted during squash build-up. Rolled back.",
                    short_oid(oid),
                ));
            }
            Err(CherryPickError::Other(e)) => {
                return rollback(format!("cherry-pick {}: {e}", short_oid(oid)));
            }
        }
    }

    // Single commit with the user's message. `--allow-empty` covers the
    // degenerate "these commits cancel each other out" case so the user
    // still gets a marker commit instead of a silent no-op.
    if let Err(e) = super::cli::GitCommand::new(repo_path)
        .args(["commit", "--allow-empty", "--no-verify", "-F", "-"])
        .stdin(new_message.as_bytes().to_vec())
        .run()
    {
        return rollback(format!("commit squashed snapshot: {e:#}"));
    }

    // ---- 7. Replay the "keep" commits on top ----
    for oid in &keep_ordered {
        let output = super::cli::GitCommand::new(repo_path)
            .args(["cherry-pick", "--allow-empty", &oid.to_string()])
            .run_raw();
        let success = matches!(&output, Ok(o) if o.status.success());
        if !success {
            return rollback(format!(
                "Cherry-pick of {} (post-squash replay) conflicted. Rolled back.",
                short_oid(oid),
            ));
        }
    }

    // ---- 8. Move the branch ref ----
    let new_head = match super::cli::run_line(repo_path, ["rev-parse", "HEAD"]) {
        Ok(s) => match gix::ObjectId::from_hex(s.trim().as_bytes()) {
            Ok(oid) => oid,
            Err(e) => return rollback(format!("parse new HEAD: {e}")),
        },
        Err(e) => return rollback(format!("rev-parse new HEAD: {e:#}")),
    };

    if let Some(ref b) = branch_name {
        // `branch -f` refuses to clobber a branch that is checked out;
        // we're detached right now so this is safe. After moving the ref
        // we `checkout` it so the user lands back on the named branch.
        if let Err(e) = super::cli::run(
            repo_path,
            ["branch", "-f", b.as_str(), &new_head.to_string()],
        ) {
            return rollback(format!("branch -f {b}: {e:#}"));
        }
        if let Err(e) = super::cli::run(repo_path, ["checkout", b.as_str()]) {
            return rollback(format!("checkout {b}: {e:#}"));
        }
    }

    // Restore stash last — the branch state is already the desired one,
    // and a conflicted stash-pop leaves diagnostics in the working tree
    // that the user can resolve without history being wrong.
    if stashed {
        let _ = super::cli::run(repo_path, ["stash", "pop"]);
    }

    SquashOutcome::Success {
        new_head_oid: new_head,
        backup_tag,
    }
}

/// Build the backup-tag ref name from a Unix timestamp (seconds since
/// epoch, UTC). Split out of the main flow so tests can lock in the
/// naming contract without reaching for real wallclock time.
///
/// Format: `mergefox/basket-squash/<YYYYMMDDTHHMMSSZ>`. The prefix is a
/// namespace so retention tooling (and the user) can easily grep them
/// out of `git tag -l 'mergefox/*'`. The timestamp is collision-safe to
/// one-second resolution across a single developer; we never expect
/// two concurrent squashes.
///
/// WHY a hand-rolled formatter instead of `chrono` / `time`: mergefox
/// doesn't currently pull either crate in, and this is the only spot
/// in the codebase that needs a civil date from a Unix timestamp. The
/// civil_from_days algorithm (Howard Hinnant's public-domain date
/// library) is pure arithmetic, correct for any year representable by
/// u64 seconds, and trivial to unit-test.
pub fn backup_tag_name(unix_seconds: u64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix_seconds(unix_seconds);
    format!(
        "mergefox/basket-squash/{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        y, mo, d, h, mi, s
    )
}

/// Convert Unix epoch seconds (UTC) to `(year, month, day, hour, min, sec)`.
/// Pure arithmetic — no locale, no TZ database. Based on Hinnant's
/// civil_from_days; correct for all years 1970+.
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

fn maybe_pop_stash(repo_path: &Path, stashed: bool) {
    if stashed {
        let _ = super::cli::run(repo_path, ["stash", "pop"]);
    }
}

fn current_branch(repo_path: &Path) -> Option<String> {
    // `symbolic-ref --short HEAD` prints the branch name on a normal
    // checkout and fails (non-zero) on detached HEAD. We want the
    // failure-as-None semantics, so we use run() and map Err→None.
    let line = super::cli::run_line(repo_path, ["symbolic-ref", "--short", "HEAD"]).ok()?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Return Err if any commit in `commits` is a merge (has >1 parent).
/// Pure-ish: consults `git rev-list --parents --no-walk` so the caller
/// can operate without constructing a full graph walk.
fn reject_merge_commits(
    repo_path: &Path,
    commits: &[gix::ObjectId],
) -> std::result::Result<(), String> {
    let mut args: Vec<String> = vec![
        "rev-list".into(),
        "--parents".into(),
        "--no-walk".into(),
    ];
    args.extend(commits.iter().map(|o| o.to_string()));
    let out = super::cli::run(repo_path, args.iter().map(String::as_str))
        .map_err(|e| format!("rev-list --parents: {e:#}"))?;
    for line in out.stdout_str().lines() {
        // Each line: `<commit> <parent1> [<parent2> ...]`. >1 parent →
        // >2 fields → merge commit.
        let n = line.split_whitespace().count();
        if n > 2 {
            let commit = line.split_whitespace().next().unwrap_or("");
            return Err(format!(
                "Commit {} is a merge — basket squash refuses merge commits (v1 limitation). \
                 Deselect it and retry.",
                &commit.get(..7.min(commit.len())).unwrap_or(commit),
            ));
        }
    }
    Ok(())
}

/// Confirm every commit in `commits` is reachable from `head`. If any
/// aren't, the "rebase the rest of the branch" step has no meaning and
/// we refuse early.
fn ensure_all_ancestors_of(
    repo_path: &Path,
    commits: &[gix::ObjectId],
    head: &gix::ObjectId,
) -> std::result::Result<(), String> {
    for oid in commits {
        let res = super::cli::GitCommand::new(repo_path)
            .args([
                "merge-base",
                "--is-ancestor",
                &oid.to_string(),
                &head.to_string(),
            ])
            .run_raw();
        match res {
            Ok(out) if out.status.success() => {}
            Ok(_) => {
                return Err(format!(
                    "Commit {} is not in HEAD's history — basket squash only rewrites the \
                     current branch.",
                    short_oid(oid),
                ));
            }
            Err(e) => return Err(format!("merge-base --is-ancestor: {e:#}")),
        }
    }
    Ok(())
}

/// Enumerate commits reachable from `head` but not from `base`, minus
/// any commit in `exclude`. Return them in apply order (oldest first)
/// so cherry-pick stacks them on top of the squashed commit.
fn commits_between_excluding(
    repo_path: &Path,
    base: &gix::ObjectId,
    head: &gix::ObjectId,
    exclude: &[gix::ObjectId],
) -> Result<Vec<gix::ObjectId>> {
    // `rev-list --topo-order --reverse base..head` gives us exactly
    // "commits introduced on top of base, ancestors-first", including
    // any merges. We drop merges (they'd need `-m` during cherry-pick
    // and the basket-squash semantics don't accommodate replaying
    // a merge on a rewritten tip). We also drop anything in `exclude`
    // — those are the basket commits we just squashed.
    let args = vec![
        "rev-list".to_string(),
        "--topo-order".to_string(),
        "--reverse".to_string(),
        "--no-merges".to_string(),
        format!("{}..{}", base, head),
    ];
    let out = super::cli::run(repo_path, args.iter().map(String::as_str))
        .context("git rev-list base..head")?;
    let mut kept = Vec::new();
    for line in out.stdout_str().lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let oid = gix::ObjectId::from_hex(trimmed.as_bytes())
            .with_context(|| format!("parse rev-list line '{trimmed}'"))?;
        if !exclude.contains(&oid) {
            kept.push(oid);
        }
    }
    Ok(kept)
}

/// Compose a default squash message from the basket commits. The UI
/// shows this in the confirm modal as an editable pre-fill. First line
/// is a summary count; the body is each commit's first-line summary,
/// bulleted, oldest-first.
///
/// WHY: picking "just the newest message" hides what the other commits
/// were; concatenating full bodies floods the dialog. Bulleted summaries
/// are a predictable compromise that produces usable default messages
/// even for large baskets — the user can always edit.
pub fn compose_default_squash_message(summaries: &[String]) -> String {
    if summaries.is_empty() {
        return String::new();
    }
    let mut out = format!("Squash of {} commits\n\n", summaries.len());
    for s in summaries {
        let first_line = s.lines().next().unwrap_or("").trim();
        if first_line.is_empty() {
            continue;
        }
        out.push_str("* ");
        out.push_str(first_line);
        out.push('\n');
    }
    // Strip trailing newline for tidiness in the modal textbox.
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Count commits still pending in `.git/sequencer/todo`.
fn sequencer_remaining_count(repo_path: &Path) -> Option<usize> {
    let git_dir = repo_path.join(".git");
    let todo = git_dir.join("sequencer").join("todo");
    let text = std::fs::read_to_string(&todo).ok()?;
    let count = text
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#')
        })
        .count();
    Some(count)
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

    #[test]
    fn parse_unmerged_z_handles_empty_input() {
        assert!(parse_unmerged_z(&[]).is_empty());
    }

    #[test]
    fn parse_unmerged_z_extracts_uu_and_aa_rows() {
        let mut data = Vec::new();
        data.extend_from_slice(b"UU conflict.rs");
        data.push(0);
        data.extend_from_slice(b"M  clean.rs");
        data.push(0);
        data.extend_from_slice(b"AA both-added.rs");
        data.push(0);

        let paths = parse_unmerged_z(&data);
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], PathBuf::from("conflict.rs"));
        assert_eq!(paths[1], PathBuf::from("both-added.rs"));
    }

    #[test]
    fn parse_unmerged_z_skips_rename_payload_cleanly() {
        let mut data = Vec::new();
        data.extend_from_slice(b"R  new");
        data.push(0);
        data.extend_from_slice(b"old");
        data.push(0);
        data.extend_from_slice(b"UU conflict.rs");
        data.push(0);

        let paths = parse_unmerged_z(&data);
        assert_eq!(paths, vec![PathBuf::from("conflict.rs")]);
    }

    #[test]
    fn parse_unmerged_z_recognises_all_conflict_codes() {
        for code in ["UU", "AA", "DD", "DU", "UD", "AU", "UA"] {
            let mut data = Vec::new();
            data.extend_from_slice(code.as_bytes());
            data.push(b' ');
            data.extend_from_slice(b"file.rs");
            data.push(0);
            let paths = parse_unmerged_z(&data);
            assert_eq!(paths, vec![PathBuf::from("file.rs")], "code={code}");
        }
    }

    #[test]
    fn revert_outcome_clean_carries_counts() {
        let out = RevertOutcome::Clean {
            commits_reverted: 3,
            auto_stashed: true,
        };
        match out {
            RevertOutcome::Clean {
                commits_reverted,
                auto_stashed,
            } => {
                assert_eq!(commits_reverted, 3);
                assert!(auto_stashed);
            }
            _ => panic!("expected Clean"),
        }
    }

    // ---------------- Phase 6 squash unit tests ----------------

    #[test]
    fn backup_tag_name_formats_unix_epoch_as_iso_basic() {
        // Pick any timestamp — the important invariant is the format.
        let secs = (20_561u64 * 86_400) + 9 * 3600 + 30 * 60 + 15;
        let tag = backup_tag_name(secs);
        assert!(tag.starts_with("mergefox/basket-squash/"));
        let suffix = tag.strip_prefix("mergefox/basket-squash/").unwrap();
        // Git-legal ref chars only, ASCII alphanumeric by construction.
        assert!(suffix.ends_with('Z'));
        assert!(suffix.chars().all(|c| c.is_ascii_alphanumeric()));
        // 15-char body (YYYYMMDDTHHMMSS) + trailing 'Z'.
        assert_eq!(suffix.len(), 16);
    }

    #[test]
    fn backup_tag_name_handles_epoch_itself() {
        assert_eq!(
            backup_tag_name(0),
            "mergefox/basket-squash/19700101T000000Z"
        );
    }

    #[test]
    fn civil_from_unix_seconds_matches_known_dates() {
        // Leap-year regression guard: 2000 is a century-leap year.
        assert_eq!(civil_from_unix_seconds(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(
            civil_from_unix_seconds(951_782_400),
            (2000, 2, 29, 0, 0, 0)
        );
        assert_eq!(
            civil_from_unix_seconds(1_735_689_599),
            (2024, 12, 31, 23, 59, 59)
        );
    }

    #[test]
    fn compose_default_squash_message_bullets_summaries() {
        let summaries = vec![
            "feat: add widget\n\nbody here".to_string(),
            "fix: typo".to_string(),
            "refactor: extract helper".to_string(),
        ];
        let msg = compose_default_squash_message(&summaries);
        assert!(msg.starts_with("Squash of 3 commits\n\n"));
        assert!(msg.contains("* feat: add widget"));
        assert!(msg.contains("* fix: typo"));
        assert!(msg.contains("* refactor: extract helper"));
        // Body MUST be stripped — large baskets would otherwise flood
        // the modal textbox.
        assert!(!msg.contains("body here"));
        assert!(!msg.ends_with('\n'));
    }

    #[test]
    fn compose_default_squash_message_empty_input_is_empty_string() {
        assert_eq!(compose_default_squash_message(&[]), "");
    }

    #[test]
    fn compose_default_squash_message_skips_blank_summaries() {
        let summaries = vec![
            "real commit".to_string(),
            "".to_string(),
            "\n\n".to_string(),
            "another".to_string(),
        ];
        let msg = compose_default_squash_message(&summaries);
        // Header reflects input length (caller pre-filters if needed).
        assert!(msg.starts_with("Squash of 4 commits"));
        assert_eq!(msg.matches("* ").count(), 2);
    }

    #[test]
    fn squash_outcome_success_carries_head_and_backup_tag() {
        let oid = gix::ObjectId::null(gix::hash::Kind::Sha1);
        let outcome = SquashOutcome::Success {
            new_head_oid: oid,
            backup_tag: "mergefox/basket-squash/test".to_string(),
        };
        match outcome {
            SquashOutcome::Success {
                new_head_oid,
                backup_tag,
            } => {
                assert_eq!(new_head_oid, oid);
                assert_eq!(backup_tag, "mergefox/basket-squash/test");
            }
            _ => panic!("expected Success"),
        }
    }

    #[test]
    fn squash_outcome_aborted_preserves_backup_tag_when_created() {
        let outcome = SquashOutcome::Aborted {
            reason: "conflict".to_string(),
            backup_tag_created: Some(
                "mergefox/basket-squash/20260418T000000Z".to_string(),
            ),
        };
        match outcome {
            SquashOutcome::Aborted {
                backup_tag_created: Some(tag),
                reason,
            } => {
                assert_eq!(reason, "conflict");
                assert!(tag.starts_with("mergefox/basket-squash/"));
            }
            _ => panic!("expected Aborted with backup tag"),
        }
    }

    #[test]
    fn squash_refuses_fewer_than_two_commits() {
        // Exercises the validation gate without touching git: empty
        // commit slice short-circuits before any subprocess spawn.
        let tmp = std::env::temp_dir();
        let outcome = squash_basket_into_one(&tmp, &[], "irrelevant");
        match outcome {
            SquashOutcome::Aborted {
                reason,
                backup_tag_created,
            } => {
                assert!(reason.to_lowercase().contains("two commits"));
                assert!(backup_tag_created.is_none());
            }
            _ => panic!("expected Aborted"),
        }
    }

    #[test]
    fn squash_refuses_empty_message() {
        let tmp = std::env::temp_dir();
        let a = gix::ObjectId::null(gix::hash::Kind::Sha1);
        let b = gix::ObjectId::null(gix::hash::Kind::Sha1);
        let outcome = squash_basket_into_one(&tmp, &[a, b], "   ");
        match outcome {
            SquashOutcome::Aborted {
                reason,
                backup_tag_created,
            } => {
                assert!(reason.to_lowercase().contains("empty"));
                assert!(backup_tag_created.is_none());
            }
            _ => panic!("expected Aborted"),
        }
    }
}
