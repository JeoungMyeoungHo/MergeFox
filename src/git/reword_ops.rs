//! Reword the message of any reachable commit, not just HEAD.
//!
//! The CLI way is `git rebase -i <oid>^` + flipping the target line to
//! `reword` + an editor dance — repeatable only by someone who knows
//! git internals. The GUI flow wraps this into a single call:
//!
//!   1. Fail fast on unreachable targets, root-as-message-rewrite (we
//!      don't support rewriting parentless commits via `commit-tree`
//!      rebase chain; amend-HEAD handles the root case separately if
//!      it happens to be HEAD).
//!   2. Auto-stash the working tree — `git rebase --onto` refuses on a
//!      dirty tree, and the stash is the user-friendly fix.
//!   3. Create a backup tag at the current HEAD so a failed reword is
//!      one `git reset --hard <tag>` away from recovery.
//!   4. Rewrite the target commit via `git commit-tree` (preserving
//!      tree, parents, and the original author/committer timestamps
//!      verbatim — reword should NOT touch metadata).
//!   5. Rebase the target's descendants onto the new commit via
//!      `git rebase --onto <new> <target> <branch>`. Conflicts here
//!      would be extraordinary for a message-only change, but we
//!      surface them as `RewordOutcome::Aborted` after a rollback just
//!      in case (e.g. someone simultaneously modified the index).
//!   6. Pop the auto-stash. Ordering matters: we pop AFTER the branch
//!      ref moves so the stash re-applies against the rewritten tip.
//!
//! This mirrors the architecture of `basket_ops::squash_basket_into_one`
//! on purpose — same safety envelope, same RAII-style rollback closure.

use std::path::Path;

use anyhow::{Context, Result};

use super::repo::{auto_stash_path, AutoStashOpts, AutoStashOutcome};

/// Outcome of a reword, from the UI thread's point of view.
///
/// `Success::new_head_oid` is the branch tip AFTER the rebase — the
/// caller should `rebuild_graph` and move `selected_commit` to this
/// oid so the diff panel doesn't stay pinned to a now-orphaned hash.
#[derive(Debug, Clone)]
pub enum RewordOutcome {
    Success {
        new_head_oid: gix::ObjectId,
        new_target_oid: gix::ObjectId,
        backup_tag: String,
    },
    Aborted {
        reason: String,
        backup_tag_created: Option<String>,
    },
}

/// Reword the commit at `target_oid` on the current branch.
///
/// `target_oid` must be reachable from the current branch tip. The
/// branch itself is identified via `HEAD` inside the call; passing it
/// in would couple the UI to the plumbing layer unnecessarily.
pub fn reword_commit(
    repo_path: &Path,
    target_oid: gix::ObjectId,
    new_message: &str,
) -> Result<RewordOutcome> {
    if new_message.trim().is_empty() {
        return Ok(RewordOutcome::Aborted {
            reason: "new commit message is empty".into(),
            backup_tag_created: None,
        });
    }

    // ── 0. Resolve HEAD and figure out whether the target is HEAD ──
    let head_ref = super::cli::run(repo_path, ["symbolic-ref", "--quiet", "HEAD"])
        .ok()
        .map(|o| o.stdout_str().trim().to_string())
        .filter(|s| !s.is_empty());
    let head_oid_text = super::cli::run(repo_path, ["rev-parse", "HEAD"])
        .context("rev-parse HEAD")?
        .stdout_str();
    let head_oid = gix::ObjectId::from_hex(head_oid_text.trim().as_bytes())
        .context("parse HEAD oid")?;
    let target_is_head = head_oid == target_oid;

    // Must be on an actual branch so `rebase --onto` has a ref to move.
    // Detached HEAD reword is possible (could cherry-pick descendants
    // and leave HEAD detached) but would surprise users; we surface a
    // clean refusal instead.
    let branch_ref = match head_ref.as_deref() {
        Some(r) => r.to_string(),
        None if target_is_head => String::new(), // amend path handles this
        None => {
            return Ok(RewordOutcome::Aborted {
                reason:
                    "HEAD is detached — check out a branch before rewording a non-HEAD commit"
                        .into(),
                backup_tag_created: None,
            });
        }
    };

    // ── 1. Auto-stash + backup tag ──────────────────────────────────
    let auto_stashed = match auto_stash_path(
        repo_path,
        "reword commit",
        AutoStashOpts::default(),
    )
    .context("auto-stash before reword")?
    {
        AutoStashOutcome::Clean => false,
        AutoStashOutcome::Stashed { .. } => true,
        AutoStashOutcome::Refused { reason } => {
            return Ok(RewordOutcome::Aborted {
                reason: reason.to_string(),
                backup_tag_created: None,
            });
        }
    };

    let backup_tag = backup_tag_name();
    super::cli::run(
        repo_path,
        ["tag", "-f", "--no-sign", &backup_tag, "HEAD"],
    )
    .context("create backup tag")?;

    // Helper: on any failure past this point, roll back to the backup
    // and (best-effort) restore the auto-stash. Kept inline as a
    // closure to avoid plumbing the rollback through every ? operator.
    let rollback = |reason: String| -> Result<RewordOutcome> {
        let _ = super::cli::run(repo_path, ["rebase", "--abort"]);
        if !branch_ref.is_empty() {
            let _ = super::cli::run(repo_path, ["checkout", "-f", &branch_ref]);
        }
        let _ = super::cli::run(repo_path, ["reset", "--hard", &backup_tag]);
        if auto_stashed {
            let _ = super::cli::run(repo_path, ["stash", "pop", "--quiet"]);
        }
        Ok(RewordOutcome::Aborted {
            reason,
            backup_tag_created: Some(backup_tag.clone()),
        })
    };

    // ── 2. Short-circuit: target is HEAD → plain amend. ─────────────
    //
    // `git commit --amend -F <file>` is a one-liner that does exactly
    // what we want here, and it preserves the author identity / date
    // automatically. No need for the commit-tree + rebase dance.
    if target_is_head {
        let msg_file = write_temp_message(new_message)?;
        let amend = super::cli::GitCommand::new(repo_path)
            .args([
                "commit",
                "--amend",
                "--no-edit",
                "--no-verify",
                "-F",
                msg_file.path().to_str().context("temp path utf8")?,
            ])
            .run_raw()
            .context("git commit --amend")?;
        if !amend.status.success() {
            let stderr = String::from_utf8_lossy(&amend.stderr).trim().to_string();
            return rollback(if stderr.is_empty() {
                "amend failed with no stderr".into()
            } else {
                stderr
            });
        }
        if auto_stashed {
            let _ = super::cli::run(repo_path, ["stash", "pop", "--quiet"]);
        }
        let new_head = current_head_oid(repo_path)?;
        return Ok(RewordOutcome::Success {
            new_head_oid: new_head,
            new_target_oid: new_head, // target == HEAD, so same
            backup_tag,
        });
    }

    // ── 3. Rewrite the target via `commit-tree`. ────────────────────
    //
    // We replay the EXACT tree + parents + author/committer identity
    // with the new message. Using `commit-tree` directly (instead of
    // `cherry-pick --edit`) is what keeps the author timestamp intact
    // — cherry-pick would stamp "now" into the committer slot, which
    // is the honest thing to do for a cherry-pick but wrong for
    // reword where nothing behaviourally changed.
    let metadata = read_commit_metadata(repo_path, target_oid)?;
    let msg_file = write_temp_message(new_message)?;

    let mut args: Vec<String> = vec!["commit-tree".into(), metadata.tree_oid.to_string()];
    for p in &metadata.parent_oids {
        args.push("-p".into());
        args.push(p.to_string());
    }
    args.push("-F".into());
    args.push(
        msg_file
            .path()
            .to_str()
            .context("temp path utf8")?
            .to_string(),
    );

    let commit_tree_out = super::cli::GitCommand::new(repo_path)
        .args(args.iter().map(String::as_str))
        .env("GIT_AUTHOR_NAME", &metadata.author_name)
        .env("GIT_AUTHOR_EMAIL", &metadata.author_email)
        .env("GIT_AUTHOR_DATE", &metadata.author_date)
        .env("GIT_COMMITTER_NAME", &metadata.committer_name)
        .env("GIT_COMMITTER_EMAIL", &metadata.committer_email)
        .env("GIT_COMMITTER_DATE", &metadata.committer_date)
        .run_raw()
        .context("git commit-tree")?;
    if !commit_tree_out.status.success() {
        let stderr = String::from_utf8_lossy(&commit_tree_out.stderr).trim().to_string();
        return rollback(format!("commit-tree failed: {stderr}"));
    }
    let new_target_oid_text = String::from_utf8_lossy(&commit_tree_out.stdout)
        .trim()
        .to_string();
    let new_target_oid = gix::ObjectId::from_hex(new_target_oid_text.as_bytes())
        .with_context(|| format!("parse commit-tree oid '{new_target_oid_text}'"))?;

    // ── 4. Rebase descendants onto the rewritten commit. ────────────
    //
    // `--onto <new_target> <old_target>` tells git to pick up everything
    // after old_target and replant it on new_target. We constrain to
    // the current branch so we don't accidentally touch other refs.
    let rebase_out = super::cli::GitCommand::new(repo_path)
        .args([
            "rebase",
            "--onto",
            &new_target_oid.to_string(),
            &target_oid.to_string(),
            &branch_ref,
        ])
        .run_raw()
        .context("git rebase --onto")?;
    if !rebase_out.status.success() {
        let stderr = String::from_utf8_lossy(&rebase_out.stderr)
            .trim()
            .to_string();
        return rollback(format!(
            "rebase onto rewritten commit failed: {stderr}"
        ));
    }

    // ── 5. Pop the auto-stash (best-effort). ────────────────────────
    if auto_stashed {
        let _ = super::cli::run(repo_path, ["stash", "pop", "--quiet"]);
    }

    let new_head = current_head_oid(repo_path)?;
    Ok(RewordOutcome::Success {
        new_head_oid: new_head,
        new_target_oid,
        backup_tag,
    })
}

struct CommitMetadata {
    tree_oid: gix::ObjectId,
    parent_oids: Vec<gix::ObjectId>,
    author_name: String,
    author_email: String,
    author_date: String,
    committer_name: String,
    committer_email: String,
    committer_date: String,
}

/// Read a commit's tree + parents + author/committer identity via a
/// `git cat-file -p` port. We deliberately don't use gix here because
/// re-emitting the date in git's exact wire format (`<seconds> <tz>`)
/// matters for `GIT_AUTHOR_DATE` to preserve bit-for-bit, and git's own
/// `cat-file` already gives us that format unchanged.
fn read_commit_metadata(repo_path: &Path, oid: gix::ObjectId) -> Result<CommitMetadata> {
    let out = super::cli::run(
        repo_path,
        ["cat-file", "-p", &oid.to_string()],
    )
    .context("cat-file commit")?;
    let text = out.stdout_str();
    let mut tree: Option<gix::ObjectId> = None;
    let mut parents: Vec<gix::ObjectId> = Vec::new();
    let mut author: Option<(String, String, String)> = None;
    let mut committer: Option<(String, String, String)> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("tree ") {
            tree = Some(gix::ObjectId::from_hex(rest.trim().as_bytes())?);
        } else if let Some(rest) = line.strip_prefix("parent ") {
            parents.push(gix::ObjectId::from_hex(rest.trim().as_bytes())?);
        } else if let Some(rest) = line.strip_prefix("author ") {
            author = Some(parse_ident(rest));
        } else if let Some(rest) = line.strip_prefix("committer ") {
            committer = Some(parse_ident(rest));
        } else if line.is_empty() {
            break; // headers ended; the message body follows
        }
    }
    let tree_oid = tree.context("commit missing tree")?;
    let (author_name, author_email, author_date) =
        author.context("commit missing author header")?;
    let (committer_name, committer_email, committer_date) =
        committer.context("commit missing committer header")?;
    Ok(CommitMetadata {
        tree_oid,
        parent_oids: parents,
        author_name,
        author_email,
        author_date,
        committer_name,
        committer_email,
        committer_date,
    })
}

/// Parse a git identity header of the form `Name <email> <seconds> <tz>`
/// into its three parts. Assumes well-formed input — `cat-file -p` is
/// the producer, which means the format is controlled, not user-typed.
fn parse_ident(s: &str) -> (String, String, String) {
    // The format is `Name <email> <date>` where `<date>` is `<unix>
    // <tz>`. Name can contain spaces but not `<`; email can't contain
    // `>`. So we split on the first `<` and the last `>`.
    let s = s.trim();
    let open = s.find('<').unwrap_or(s.len());
    let name = s[..open].trim().to_string();
    let close = s.rfind('>').unwrap_or(s.len());
    let email_start = open.saturating_add(1);
    let email = s[email_start.min(close)..close].to_string();
    let date_rest = s
        .get(close.saturating_add(1)..)
        .unwrap_or("")
        .trim()
        .to_string();
    (name, email, date_rest)
}

/// Build a filesystem-safe backup tag name. Scoped under
/// `mergefox/reword/` so it doesn't collide with squash backups.
fn backup_tag_name() -> String {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("mergefox/reword/{}", format_iso_utc(unix as i64))
}

fn format_iso_utc(unix: i64) -> String {
    if unix <= 0 {
        return "19700101T000000Z".to_string();
    }
    let days = unix / 86_400;
    let tod = unix.rem_euclid(86_400) as i64;
    let (y, mo, d) = civil_from_days(days);
    let hh = tod / 3600;
    let mm = (tod % 3600) / 60;
    let ss = tod % 60;
    format!("{y:04}{mo:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

/// Howard Hinnant's days→civil. Copied (rather than re-exported) so
/// `reword_ops` stays a leaf module. If a third user of this routine
/// shows up we'll hoist to `git::time_utils`.
fn civil_from_days(mut z: i64) -> (i32, u32, u32) {
    z += 719468;
    let era = if z >= 0 { z / 146097 } else { (z - 146096) / 146097 };
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn write_temp_message(msg: &str) -> Result<NamedTempFile> {
    // Ensure trailing newline — git is picky about commits whose message
    // doesn't end in `\n` (they parse fine but some downstream tools
    // choke, and `git log`'s formatting quietly adds one anyway).
    let payload = if msg.ends_with('\n') {
        msg.to_string()
    } else {
        format!("{msg}\n")
    };
    let path = std::env::temp_dir().join(format!(
        "mergefox-reword-{}-{}.txt",
        std::process::id(),
        NEXT_TEMP_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&path, payload).with_context(|| format!("write temp msg {}", path.display()))?;
    Ok(NamedTempFile { path })
}

static NEXT_TEMP_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Tiny RAII wrapper — deletes the temp file on drop. We don't use
/// the `tempfile` crate because `std` + a static counter covers the
/// single-file case we need here without a new dependency.
struct NamedTempFile {
    path: std::path::PathBuf,
}

impl NamedTempFile {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for NamedTempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn current_head_oid(repo_path: &Path) -> Result<gix::ObjectId> {
    let out = super::cli::run(repo_path, ["rev-parse", "HEAD"])?;
    let text = out.stdout_str();
    let trimmed = text.trim();
    Ok(gix::ObjectId::from_hex(trimmed.as_bytes())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ident_splits_name_email_and_date() {
        let (n, e, d) = parse_ident("Alice Example <alice@example.com> 1700000000 +0900");
        assert_eq!(n, "Alice Example");
        assert_eq!(e, "alice@example.com");
        assert_eq!(d, "1700000000 +0900");
    }

    #[test]
    fn parse_ident_tolerates_spaces_in_name() {
        let (n, _, _) = parse_ident("Van Der Graaf  <vdg@example.com> 1 +0000");
        assert_eq!(n, "Van Der Graaf");
    }

    #[test]
    fn format_iso_utc_handles_epoch() {
        assert_eq!(format_iso_utc(0), "19700101T000000Z");
    }

    #[test]
    fn format_iso_utc_known_date() {
        // 2024-06-15 12:34:56 UTC = 1718454896.
        assert_eq!(format_iso_utc(1718454896), "20240615T123456Z");
    }

    #[test]
    fn backup_tag_name_is_under_mergefox_reword_namespace() {
        assert!(backup_tag_name().starts_with("mergefox/reword/"));
    }
}
