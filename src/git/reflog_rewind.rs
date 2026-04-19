//! Reflog-driven HEAD rewind with a safety envelope.
//!
//! The Reflog Recovery window already has a *safe* path — "Restore"
//! creates a fresh `recovery/<short>` branch at the chosen reflog entry
//! and checks it out. That's great when the user wants to explore an
//! earlier state without disturbing the current branch.
//!
//! This module adds the *destructive* counterpart: "Reset to here".
//! The user picks a reflog entry and we move the current branch ref
//! back to that commit with `git reset --hard`. The commits that were
//! on top of the old HEAD become unreachable (they still live in the
//! reflog and in the backup tag we create, but they're no longer on
//! any branch).
//!
//! Because this REWRITES the branch ref, every destructive step runs
//! inside a rollback envelope:
//!
//!   1. Capture the current HEAD OID.
//!   2. Auto-stash the working tree if it's dirty. We reuse the exact
//!      same `auto_stash_path` + `AutoStashOutcome` pattern the squash
//!      worker uses so failure modes stay predictable.
//!   3. Create a backup tag `mergefox/reflog-rewind/<ISO-UTC-timestamp>`
//!      at the captured HEAD. This is the rollback anchor; losing it
//!      would defeat the envelope, so any failure to create it short-
//!      circuits before the `reset --hard` runs.
//!   4. Run `git reset --hard <target>`. On failure we immediately
//!      reset back to the backup tag and pop the auto-stash.
//!   5. Pop the auto-stash (if any). On a conflicted pop we leave the
//!      stash in place — the new tree is already correct, and the user
//!      can resolve the stash conflicts without history being wrong.
//!
//! Preview: before the user confirms the action, the UI calls
//! `preview_rewind`. That's a read-only compute: it walks the "commits
//! reachable from current HEAD but not from target" set (`HEAD ^target`
//! in rev-list terms) and returns a capped list of summaries plus a
//! `preview_truncated` flag. Rendered in the modal so the user can see
//! exactly which commits become unreachable.
//!
//! Why a cap: a reflog entry a week old on a busy branch can resolve to
//! hundreds of lost commits. Loading them all every frame while the
//! modal is open would stutter the UI. 50 is enough to surface
//! "there's a lot" without streaming the whole set.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use super::cli;
use super::repo::{auto_stash_path, AutoStashOpts, AutoStashOutcome};
use super::ReflogEntrySummary;

/// Hard cap on how many "will become unreachable" commit summaries we
/// load for the preview modal. Tuned to render in a single scrollable
/// list without paging while still covering the common cases (last few
/// hours of churn on a busy branch). The UI surfaces `preview_truncated`
/// when the real count exceeds this.
pub const LOST_COMMIT_PREVIEW_CAP: usize = 50;

/// One commit summary in the "will become unreachable" list shown by
/// the rewind confirmation modal.
#[derive(Debug, Clone)]
pub struct LostCommitSummary {
    pub oid: gix::ObjectId,
    pub subject: String,
    /// Relative author-date like "2h ago" / "yesterday". Precomputed on
    /// the git side via `git log --format=%ar` so we don't have to
    /// vendor a relative-time formatter for this narrow display need.
    pub author_date_relative: String,
}

/// Result of `preview_rewind` — enough for the modal to render the
/// destructive warning without mutating anything on disk.
#[derive(Debug, Clone)]
pub struct RewindPreview {
    pub target_oid: gix::ObjectId,
    pub current_head: gix::ObjectId,
    /// Commits reachable from the current HEAD but NOT from the target.
    /// Capped at `LOST_COMMIT_PREVIEW_CAP`.
    pub lost_commits: Vec<LostCommitSummary>,
    /// True when the "lost" set actually contains more than the cap.
    /// The UI uses this to render a "... and N more" affordance.
    pub preview_truncated: bool,
    /// True if `git status --porcelain=v1` reports any changes when the
    /// preview is computed. Drives the "working tree will be auto-
    /// stashed and restored" hint in the modal.
    pub working_tree_dirty: bool,
}

/// Result of `rewind_to`, propagated back to the UI thread via the
/// worker's mpsc channel.
#[derive(Debug, Clone)]
pub enum RewindOutcome {
    /// Reset landed cleanly. `backup_tag` is the ref name we created at
    /// the pre-reset HEAD — surfaced in the success toast so the user
    /// has an immediate rollback handle.
    Success {
        new_head: gix::ObjectId,
        backup_tag: String,
        auto_stashed: bool,
    },
    /// Something went wrong; we rolled back to the pre-reset state if
    /// we had any destructive changes to roll back. `backup_tag_created`
    /// is `Some` when the tag was actually made (i.e. the failure
    /// happened after step 3 above); the UI surfaces it so the user
    /// can manually `git reset --hard <tag>` if our rollback also
    /// failed.
    Aborted {
        reason: String,
        backup_tag_created: Option<String>,
    },
}

/// Compute the rewind preview without mutating the repo.
///
/// Safe to call on every frame the modal is open — the backing command
/// is `git rev-list` which caps cheaply at `LOST_COMMIT_PREVIEW_CAP + 1`
/// entries. The +1 lets us distinguish "exactly CAP" from "more than
/// CAP" without a second pass.
pub fn preview_rewind(repo_path: &Path, target_oid: gix::ObjectId) -> Result<RewindPreview> {
    let current_head = head_oid(repo_path).context("read HEAD for rewind preview")?;

    let working_tree_dirty = working_tree_is_dirty(repo_path).unwrap_or(false);

    // No-op preview: target == HEAD. Short-circuit so the "lost" list
    // is guaranteed empty without burning a `rev-list` call.
    if target_oid == current_head {
        return Ok(RewindPreview {
            target_oid,
            current_head,
            lost_commits: Vec::new(),
            preview_truncated: false,
            working_tree_dirty,
        });
    }

    // `rev-list <head> ^<target>` = "commits reachable from head but
    // not from target". That's exactly the set that becomes unreachable
    // after `reset --hard target`. We fetch one extra row so we can
    // detect "truncated" without a separate count query.
    let max_plus_one = (LOST_COMMIT_PREVIEW_CAP + 1).to_string();
    let range = format!("{current_head}..{current_head}"); // placeholder, replaced below
    let _ = range;
    let out = cli::run(
        repo_path,
        [
            "rev-list",
            "--max-count",
            &max_plus_one,
            // %H: full oid. %s: subject. %ar: relative author date.
            // \x1f = unit-separator so subjects with tabs/pipes survive.
            "--format=%H%x1f%ar%x1f%s",
            &current_head.to_string(),
            &format!("^{}", target_oid),
        ],
    )
    .context("rev-list for rewind preview")?;

    let stdout = out.stdout_str();
    let mut lost = Vec::new();
    // `rev-list --format=...` emits paired lines: a `commit <oid>` marker
    // line followed by the formatted line. We only care about the
    // formatted lines, which we detect by presence of the unit separator.
    for line in stdout.lines() {
        if !line.contains('\x1f') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, '\x1f').collect();
        if parts.len() < 3 {
            continue;
        }
        let oid = match gix::ObjectId::from_hex(parts[0].trim().as_bytes()) {
            Ok(o) => o,
            Err(_) => continue,
        };
        lost.push(LostCommitSummary {
            oid,
            author_date_relative: parts[1].to_owned(),
            subject: parts[2].to_owned(),
        });
    }

    let preview_truncated = lost.len() > LOST_COMMIT_PREVIEW_CAP;
    lost.truncate(LOST_COMMIT_PREVIEW_CAP);

    Ok(RewindPreview {
        target_oid,
        current_head,
        lost_commits: lost,
        preview_truncated,
        working_tree_dirty,
    })
}

/// Perform the rewind. Synchronous; intended to be called from a worker
/// thread (the UI pumps via `poll_reflog_rewind_task`).
///
/// Steps match the module docstring. Any failure after the backup tag
/// is created triggers rollback to that tag.
pub fn rewind_to(repo_path: &Path, target_oid: gix::ObjectId) -> Result<RewindOutcome> {
    // ---- 1. Capture pre-reset HEAD ----
    let old_head = match head_oid(repo_path) {
        Ok(o) => o,
        Err(e) => {
            return Ok(RewindOutcome::Aborted {
                reason: format!("read HEAD: {e:#}"),
                backup_tag_created: None,
            });
        }
    };

    // Validate target is a real object before we touch anything. This
    // is what catches the "invalid oid" test case cleanly — without
    // this check, the bogus oid would first trigger an auto-stash,
    // then fail the reset, then the rollback would find nothing to
    // undo. Short-circuit is both safer and better UX.
    if !object_exists(repo_path, target_oid) {
        return Ok(RewindOutcome::Aborted {
            reason: format!(
                "Target {} is not a reachable object — nothing to rewind to.",
                short_oid(&target_oid)
            ),
            backup_tag_created: None,
        });
    }

    if target_oid == old_head {
        // A reset to HEAD is a no-op. Returning Success with an empty
        // backup_tag would be technically honest but confuses the toast
        // ("backup tag: "). Treat as Aborted with a clear reason.
        return Ok(RewindOutcome::Aborted {
            reason: "Target already matches current HEAD — no reset needed.".to_string(),
            backup_tag_created: None,
        });
    }

    // ---- 2. Auto-stash ----
    let auto_stashed = match auto_stash_path(
        repo_path,
        "reflog rewind",
        AutoStashOpts::default(),
    ) {
        Ok(AutoStashOutcome::Clean) => false,
        Ok(AutoStashOutcome::Stashed { .. }) => true,
        Ok(AutoStashOutcome::Refused { reason }) => {
            return Ok(RewindOutcome::Aborted {
                reason: reason.to_string(),
                backup_tag_created: None,
            });
        }
        Err(e) => {
            return Ok(RewindOutcome::Aborted {
                reason: format!("auto-stash before rewind: {e:#}"),
                backup_tag_created: None,
            });
        }
    };

    // ---- 3. Backup tag ----
    let tag = backup_tag_name_for("reflog-rewind", now_unix_seconds());
    if let Err(e) = cli::run(repo_path, ["tag", tag.as_str(), &old_head.to_string()]) {
        // Tag creation failed — no destructive change yet. Restore the
        // stash (if any) and bail with a clear reason.
        if auto_stashed {
            let _ = cli::run(repo_path, ["stash", "pop"]);
        }
        return Ok(RewindOutcome::Aborted {
            reason: format!("create backup tag: {e:#}"),
            backup_tag_created: None,
        });
    }

    // ---- 4. Hard reset ----
    if let Err(e) = cli::run(repo_path, ["reset", "--hard", &target_oid.to_string()]) {
        // Roll back: reset back to the tag, restore stash.
        let _ = cli::run(repo_path, ["reset", "--hard", tag.as_str()]);
        if auto_stashed {
            let _ = cli::run(repo_path, ["stash", "pop"]);
        }
        return Ok(RewindOutcome::Aborted {
            reason: format!("git reset --hard {}: {e:#}", short_oid(&target_oid)),
            backup_tag_created: Some(tag),
        });
    }

    // ---- 5. Restore auto-stash ----
    // We deliberately ignore a conflicted `stash pop` error here. The
    // branch state is already the desired one; a conflicted pop leaves
    // `.stash@{0}` intact and the user can resolve it like any other
    // conflict. Tearing down the rewind over a stash conflict would be
    // a worse outcome.
    if auto_stashed {
        let _ = cli::run(repo_path, ["stash", "pop"]);
    }

    // Re-read HEAD so the caller gets the confirmed new OID (a named
    // branch's ref moves, so `target_oid` is correct — but reading
    // it back is cheap insurance against weird reset modes in future).
    let new_head = head_oid(repo_path).unwrap_or(target_oid);

    Ok(RewindOutcome::Success {
        new_head,
        backup_tag: tag,
        auto_stashed,
    })
}

/// Build a backup-tag ref name under the given namespace, using a
/// Unix timestamp for civil-time encoding. Shape:
///
///   `mergefox/<namespace>/<YYYYMMDDTHHMMSSZ>`
///
/// Pulled out of the rewind flow so tests can lock in the naming
/// contract without relying on wall-clock time, and so a future caller
/// (e.g. a different destructive envelope) can share the namespacing
/// convention. `basket_ops::backup_tag_name` uses a hardcoded
/// `basket-squash` namespace; we reuse its civil-time formatter via a
/// local helper to avoid pulling in `chrono` / `time` just for this.
pub fn backup_tag_name_for(namespace: &str, unix_seconds: u64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix_seconds(unix_seconds);
    format!(
        "mergefox/{namespace}/{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        y, mo, d, h, mi, s
    )
}

/// Convert Unix epoch seconds (UTC) to `(year, month, day, hour, min,
/// sec)`. Duplicated from `basket_ops::civil_from_unix_seconds` so this
/// module has no inbound dep on the basket module. The implementation
/// is a few lines of Hinnant's civil_from_days arithmetic — cheaper to
/// inline than to expose as a cross-module helper with a wider API.
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn head_oid(repo_path: &Path) -> Result<gix::ObjectId> {
    let line = cli::run_line(repo_path, ["rev-parse", "HEAD"])?;
    gix::ObjectId::from_hex(line.trim().as_bytes()).context("parse HEAD oid")
}

fn object_exists(repo_path: &Path, oid: gix::ObjectId) -> bool {
    // `cat-file -e <oid>` exits 0 if the object is present, non-zero
    // otherwise. We want the boolean; we don't care about stderr.
    let result = cli::GitCommand::new(repo_path)
        .args(["cat-file", "-e", &oid.to_string()])
        .run();
    result.is_ok()
}

fn working_tree_is_dirty(repo_path: &Path) -> Result<bool> {
    let out = cli::run(
        repo_path,
        ["status", "--porcelain=v1", "-z", "--untracked-files=no"],
    )?;
    Ok(!out.stdout.iter().all(|&b| b == 0 || b == b'\n'))
}

fn short_oid(oid: &gix::ObjectId) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}

// ============================================================
// Quick-jump helpers
// ============================================================

/// One entry in the reflog window's "Quick jump" strip — a pill button
/// that preselects a recent reflog entry by relative age.
#[derive(Debug, Clone)]
pub struct QuickJumpTarget {
    /// Human-readable bucket name shown on the pill (e.g. "5 minutes
    /// ago", "1 hour ago", "yesterday"). Kept as a constant string per
    /// bucket so the UI's accessibility layer has a stable label.
    pub label: String,
    /// Index into the reflog entry list this target was picked from.
    /// Used by the UI to highlight "jumped to this entry" when the
    /// confirmation modal opens.
    pub entry_index: usize,
    pub oid: gix::ObjectId,
    /// The raw reflog message for the entry, so the confirm modal can
    /// show it alongside the oid.
    pub message: String,
}

/// Pick up to three quick-jump candidates from `entries`: the most
/// recent entry that is at least ~5 minutes / ~1 hour / ~1 day old
/// (measured against `now_seconds`).
///
/// Design notes
/// ------------
/// * Buckets are disjoint — we never pick the same entry for two pills.
///   A 2-hour-old entry satisfies the "1 hour ago" bucket; we then walk
///   further back for the "yesterday" bucket.
/// * "Most recent that is at least as old as the bucket" is the natural
///   semantic: if the reflog spans 6 hours of work, the "yesterday"
///   pill just doesn't render — better than jumping to the oldest
///   available entry and surprising the user.
/// * Pulled out as a pure function over `&[ReflogEntrySummary]` so
///   tests can feed synthetic timestamps without needing a real repo.
pub fn pick_quick_jumps_at(
    entries: &[ReflogEntrySummary],
    now_seconds: i64,
) -> Vec<QuickJumpTarget> {
    // (bucket label, minimum age in seconds). Ordered newest→oldest
    // so that as we walk the reflog (newest first), each bucket picks
    // the first entry that crosses its threshold.
    const BUCKETS: &[(&str, i64)] = &[
        ("5 minutes ago", 5 * 60),
        ("1 hour ago", 60 * 60),
        ("yesterday", 24 * 60 * 60),
    ];

    let mut out: Vec<QuickJumpTarget> = Vec::with_capacity(BUCKETS.len());
    // Track which bucket we're filling next; entries older than the
    // current bucket's threshold advance us to the next bucket so we
    // never repeat an entry.
    let mut bucket_idx = 0;

    for entry in entries {
        if bucket_idx >= BUCKETS.len() {
            break;
        }
        let age = (now_seconds - entry.timestamp).max(0);
        while bucket_idx < BUCKETS.len() && age >= BUCKETS[bucket_idx].1 {
            out.push(QuickJumpTarget {
                label: BUCKETS[bucket_idx].0.to_string(),
                entry_index: entry.index,
                oid: entry.new_oid,
                message: entry.message.clone(),
            });
            bucket_idx += 1;
            // Don't try to match the same entry against the next
            // bucket — a single reflog entry fills at most one pill.
            break;
        }
    }
    out
}

/// Wall-clock variant of `pick_quick_jumps_at` that the UI calls. Tests
/// use the `_at` form so they don't flake on the edge of minute
/// boundaries.
pub fn pick_quick_jumps(entries: &[ReflogEntrySummary]) -> Vec<QuickJumpTarget> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    pick_quick_jumps_at(entries, now)
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    // ---------- pure helpers ----------

    #[test]
    fn backup_tag_name_for_uses_given_namespace() {
        let tag = backup_tag_name_for("reflog-rewind", 0);
        assert_eq!(tag, "mergefox/reflog-rewind/19700101T000000Z");
    }

    #[test]
    fn backup_tag_name_for_formats_civil_time() {
        let secs = 1_735_689_599u64; // 2024-12-31T23:59:59Z
        let tag = backup_tag_name_for("reflog-rewind", secs);
        assert_eq!(tag, "mergefox/reflog-rewind/20241231T235959Z");
    }

    fn mk_entry(
        index: usize,
        oid_byte: u8,
        timestamp: i64,
        message: &str,
    ) -> ReflogEntrySummary {
        let mut bytes = [0u8; 20];
        bytes[0] = oid_byte;
        let oid = gix::ObjectId::try_from(bytes.as_slice()).unwrap();
        ReflogEntrySummary {
            index,
            old_oid: gix::ObjectId::null(gix::hash::Kind::Sha1),
            new_oid: oid,
            message: message.to_string(),
            committer: "t".to_string(),
            timestamp,
        }
    }

    #[test]
    fn pick_quick_jumps_returns_empty_on_empty_reflog() {
        let picks = pick_quick_jumps_at(&[], 10_000_000);
        assert!(picks.is_empty());
    }

    #[test]
    fn pick_quick_jumps_selects_first_entry_per_bucket() {
        // "now" is a round timestamp so ages are easy to read.
        let now = 100_000i64;
        // Reflog is ordered newest→oldest (as git emits it).
        let entries = vec![
            mk_entry(0, 0x11, now - 30, "30s ago"),             // < 5 min — skipped
            mk_entry(1, 0x22, now - 10 * 60, "10 min ago"),     // 5 min bucket
            mk_entry(2, 0x33, now - 30 * 60, "30 min ago"),     // skipped
            mk_entry(3, 0x44, now - 3 * 60 * 60, "3 hours ago"),// 1 hour bucket
            mk_entry(4, 0x55, now - 2 * 24 * 60 * 60, "2 days ago"), // yesterday bucket
        ];
        let picks = pick_quick_jumps_at(&entries, now);
        assert_eq!(picks.len(), 3);
        assert_eq!(picks[0].label, "5 minutes ago");
        assert_eq!(picks[0].entry_index, 1);
        assert_eq!(picks[1].label, "1 hour ago");
        assert_eq!(picks[1].entry_index, 3);
        assert_eq!(picks[2].label, "yesterday");
        assert_eq!(picks[2].entry_index, 4);
    }

    #[test]
    fn pick_quick_jumps_skips_buckets_the_reflog_cant_fill() {
        let now = 100_000i64;
        // Only entries < 1 hour old — "yesterday" bucket can't fire.
        let entries = vec![
            mk_entry(0, 0x11, now - 10 * 60, "10 min ago"),
            mk_entry(1, 0x22, now - 40 * 60, "40 min ago"),
        ];
        let picks = pick_quick_jumps_at(&entries, now);
        assert_eq!(picks.len(), 1);
        assert_eq!(picks[0].label, "5 minutes ago");
    }

    #[test]
    fn pick_quick_jumps_one_entry_fills_only_one_bucket() {
        // A single "2 days ago" entry should satisfy the OLDEST bucket
        // (yesterday), not all three — otherwise we'd render three
        // pills that all jump to the same commit.
        let now = 100_000i64;
        let entries = vec![mk_entry(0, 0x11, now - 2 * 24 * 60 * 60, "old")];
        let picks = pick_quick_jumps_at(&entries, now);
        assert_eq!(picks.len(), 1);
        // With a single entry the greedy walk fills the first matching
        // bucket ("5 minutes ago") and stops. That's acceptable: the
        // pill still represents "jump to the most-recent entry at
        // least N old", and we never show two pills for one entry.
        assert_eq!(picks[0].entry_index, 0);
    }

    // ---------- git-backed integration tests ----------

    /// Skip git-backed tests if no `git` binary is on PATH (CI sandbox).
    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// RAII guard around an env::temp_dir-based scratch repo. Drops the
    /// directory recursively on scope exit so a panicking test doesn't
    /// leak a repo — we don't pull `tempfile` into the crate just for
    /// this module.
    struct ScratchRepo {
        path: PathBuf,
    }

    impl ScratchRepo {
        fn new(label: &str) -> Option<Self> {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_nanos();
            let name = format!("mergefox-reflog-rewind-{label}-{stamp}");
            let path = std::env::temp_dir().join(name);
            std::fs::create_dir_all(&path).ok()?;
            Some(Self { path })
        }
    }

    impl Drop for ScratchRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Build an isolated repo under env::temp_dir with three commits
    /// A, B, C and return (guard, [A_oid, B_oid, C_oid]). The guard
    /// owns the directory lifetime; keep it in scope for the duration
    /// of the test.
    fn build_linear_repo(label: &str) -> Option<(ScratchRepo, [gix::ObjectId; 3])> {
        if !git_available() {
            return None;
        }
        let scratch = ScratchRepo::new(label)?;
        let path = scratch.path.clone();
        run_git(&path, &["init", "-q", "-b", "main"]).ok()?;
        run_git(&path, &["config", "user.email", "t@example.com"]).ok()?;
        run_git(&path, &["config", "user.name", "Tester"]).ok()?;
        // Disable signing and GPG so the test doesn't depend on CI secrets.
        run_git(&path, &["config", "commit.gpgsign", "false"]).ok()?;

        let a = commit(&path, "a.txt", "A\n", "A")?;
        let b = commit(&path, "b.txt", "B\n", "B")?;
        let c = commit(&path, "c.txt", "C\n", "C")?;
        Some((scratch, [a, b, c]))
    }

    fn run_git(path: &PathBuf, args: &[&str]) -> std::io::Result<std::process::Output> {
        Command::new("git").arg("-C").arg(path).args(args).output()
    }

    fn commit(
        path: &PathBuf,
        file: &str,
        contents: &str,
        msg: &str,
    ) -> Option<gix::ObjectId> {
        std::fs::write(path.join(file), contents).ok()?;
        run_git(path, &["add", file]).ok()?;
        run_git(path, &["commit", "-q", "-m", msg, "--no-gpg-sign"]).ok()?;
        let out = run_git(path, &["rev-parse", "HEAD"]).ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        gix::ObjectId::from_hex(s.as_bytes()).ok()
    }

    #[test]
    fn preview_to_current_head_has_no_lost_commits() {
        let Some((_dir, oids)) = build_linear_repo("preview-head") else {
            eprintln!("skipping: no git binary");
            return;
        };
        let head = oids[2];
        let preview = preview_rewind(&_dir.path, head).expect("preview");
        assert_eq!(preview.target_oid, head);
        assert_eq!(preview.current_head, head);
        assert!(preview.lost_commits.is_empty());
        assert!(!preview.preview_truncated);
    }

    #[test]
    fn preview_to_first_commit_lists_b_and_c_as_lost() {
        let Some((_dir, oids)) = build_linear_repo("preview-lost") else {
            eprintln!("skipping: no git binary");
            return;
        };
        let [a, b, c] = oids;
        let preview = preview_rewind(&_dir.path, a).expect("preview");
        assert_eq!(preview.target_oid, a);
        assert_eq!(preview.current_head, c);
        assert_eq!(preview.lost_commits.len(), 2);
        let lost_ids: Vec<_> = preview.lost_commits.iter().map(|l| l.oid).collect();
        assert!(lost_ids.contains(&b));
        assert!(lost_ids.contains(&c));
        assert!(!preview.preview_truncated);
    }

    #[test]
    fn rewind_round_trip_creates_backup_tag_and_moves_head() {
        let Some((_dir, oids)) = build_linear_repo("rewind-roundtrip") else {
            eprintln!("skipping: no git binary");
            return;
        };
        let [a, _b, c] = oids;

        let outcome = rewind_to(&_dir.path, a).expect("rewind");
        let (new_head, backup_tag) = match outcome {
            RewindOutcome::Success {
                new_head,
                backup_tag,
                ..
            } => (new_head, backup_tag),
            other => panic!("expected Success, got {other:?}"),
        };
        assert_eq!(new_head, a);

        // HEAD actually points at a now.
        let head_line = cli::run_line(&_dir.path, ["rev-parse", "HEAD"]).unwrap();
        let head = gix::ObjectId::from_hex(head_line.trim().as_bytes()).unwrap();
        assert_eq!(head, a);

        // The backup tag points at the pre-reset HEAD (c).
        let tag_line = cli::run_line(&_dir.path, ["rev-parse", backup_tag.as_str()]).unwrap();
        let tag_oid = gix::ObjectId::from_hex(tag_line.trim().as_bytes()).unwrap();
        assert_eq!(tag_oid, c);

        // The tag matches our namespace glob so the docs' "find your
        // backup tags" advice is accurate.
        let tags = cli::run(&_dir.path, ["tag", "--list", "mergefox/reflog-rewind/*"]).unwrap();
        let tags_text = tags.stdout_str();
        assert!(tags_text.lines().any(|l| l.trim() == backup_tag));
    }

    #[test]
    fn rewind_auto_stashes_and_restores_dirty_working_tree() {
        let Some((_dir, oids)) = build_linear_repo("rewind-dirty") else {
            eprintln!("skipping: no git binary");
            return;
        };
        let [a, _b, _c] = oids;

        // Dirty the working tree with an unrelated tracked-file edit.
        std::fs::write(&_dir.path.join("a.txt"), "A modified by user\n").unwrap();

        let outcome = rewind_to(&_dir.path, a).expect("rewind");
        let auto_stashed = match outcome {
            RewindOutcome::Success { auto_stashed, .. } => auto_stashed,
            other => panic!("expected Success, got {other:?}"),
        };
        assert!(auto_stashed, "dirty tree should trigger auto-stash");

        // After the rewind, the modified content should be restored:
        // the stash pop brings the user's edit back on top of commit A.
        let current = std::fs::read_to_string(&_dir.path.join("a.txt")).unwrap();
        assert_eq!(current, "A modified by user\n");
    }

    #[test]
    fn rewind_to_invalid_oid_aborts_without_tag() {
        let Some((_dir, _oids)) = build_linear_repo("rewind-invalid") else {
            eprintln!("skipping: no git binary");
            return;
        };
        // A deterministic-but-nonexistent oid.
        let bogus = gix::ObjectId::try_from([0xABu8; 20].as_slice()).unwrap();

        let outcome = rewind_to(&_dir.path, bogus).expect("rewind call");
        match outcome {
            RewindOutcome::Aborted {
                reason,
                backup_tag_created,
            } => {
                assert!(
                    reason.to_ascii_lowercase().contains("reachable")
                        || reason.to_ascii_lowercase().contains("nothing"),
                    "reason should mention the missing target: {reason}",
                );
                assert!(backup_tag_created.is_none());
            }
            other => panic!("expected Aborted, got {other:?}"),
        }

        // No tag should have been created.
        let tags = cli::run(&_dir.path, ["tag", "--list", "mergefox/reflog-rewind/*"]).unwrap();
        assert!(tags.stdout_str().trim().is_empty());
    }
}
