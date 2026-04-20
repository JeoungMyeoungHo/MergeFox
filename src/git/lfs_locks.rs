//! Git LFS file-lock operations (`git lfs lock / unlock / locks`).
//!
//! ## Why locks matter
//!
//! Teams coming to Git from Perforce for game / art asset workflows
//! need a way to coordinate edits on files that aren't textually
//! mergeable — a `.psd`, `.uasset` or `.fbx` doesn't three-way-merge
//! the way source code does, so two developers editing the same asset
//! in parallel means someone's work gets overwritten. Git LFS ships a
//! locking protocol precisely for this: before you start editing a
//! binary asset you `git lfs lock path/to/asset.psd`, other users see
//! the lock when they fetch, and you release it with `git lfs unlock`
//! after pushing your edit.
//!
//! ## Why shell out instead of parsing the server protocol
//!
//! The LFS lock server is a separate HTTP service (typically the
//! hosting provider — GitHub, GitLab, Gitea, self-hosted gitlab-lfs,
//! or an on-prem lfs-test-server). Implementing the client protocol
//! ourselves would mean re-doing credential plumbing, retry logic,
//! and server-specific auth quirks (proxies, bearer tokens, NTLM).
//! `git lfs` already does all of that and respects the user's
//! `.lfsconfig` + `core.askPass` + credential-helper setup. Shelling
//! out keeps MergeFox locks working in every environment where the
//! user's terminal `git lfs lock` already works.
//!
//! ## Listing: JSON instead of text
//!
//! `git lfs locks --json` emits a machine-stable JSON array. The
//! human-readable form re-flows columns depending on terminal width
//! and quotes paths inconsistently across platforms. Parse the JSON.
//!
//! ## "Unavailable" vs "Error"
//!
//! An LFS lock call can fail for two very different reasons:
//!
//! 1. The user's repo doesn't use LFS at all, or the remote doesn't
//!    speak the locks API. This is the common case for most repos
//!    and isn't a problem the user needs to fix — the UI should just
//!    keep the lock panel tucked away.
//! 2. Something genuinely went wrong (network error, auth failure,
//!    lock conflict). The UI wants to surface these so the user can
//!    react.
//!
//! We classify case 1 into [`LfsListResult::Unavailable`] so the UI
//! can render an inline explanation instead of an error toast, and
//! only return `Err` for case 2.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// A single lock held on the remote. Fields we carry are the subset
/// the UI actually renders; the raw JSON carries more (lock expiry,
/// server-assigned refs) that we ignore for now to keep the struct
/// small and the parser forgiving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsLock {
    /// Server-assigned lock id. Needed by some servers for unlock by
    /// id (we currently unlock by path, but the id is handy for
    /// deduplication / tooltip display).
    pub id: String,
    /// Repo-relative path of the locked file. Normalised on parse
    /// (forward slashes collapsed via `PathBuf`). Empty paths are
    /// dropped before this struct is constructed.
    pub path: PathBuf,
    /// Owner username as reported by the lock server (e.g. `alice`
    /// on GitHub). Comparing this to the local `git config user.name`
    /// tells the UI whether the current user owns the lock; the
    /// match is done in the caller so this module stays I/O-free for
    /// that question.
    pub owner: String,
    /// ISO-8601 timestamp the lock was created, when the server
    /// reports it. Several server implementations omit the field for
    /// very old locks, so we keep it optional rather than forcing a
    /// placeholder.
    pub locked_at: Option<String>,
}

/// Outcome of [`list_locks`]. The `Unavailable` arm exists so the UI
/// can distinguish "nothing to show for this repo" from "real error".
/// See module-level comment for the rationale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LfsListResult {
    /// LFS is configured and the server responded. `locks` may be
    /// empty — that just means nothing is locked right now.
    Ok { locks: Vec<LfsLock> },
    /// LFS isn't configured for this repo (no lfs remote, no
    /// `.lfsconfig`, git-lfs extension not installed, or the remote
    /// doesn't implement the locks API). The UI should render an
    /// inline explanation, not an error toast.
    Unavailable { reason: String },
}

/// Specific error variants surfaced by [`unlock`]. The UI uses
/// `NotOwner` to offer a "Force unlock" affordance instead of a raw
/// error message.
#[derive(Debug)]
pub enum LfsUnlockError {
    /// The current user doesn't own this lock. Pass `force = true`
    /// to unlock it anyway (requires server-side admin permission —
    /// the server will still reject the force if the current user
    /// isn't an admin, which surfaces as a generic `Other`).
    NotOwner,
    /// LFS not installed / not configured for this repo. Distinct
    /// from `Other` so callers can hide error toasts when the repo
    /// legitimately doesn't participate in locking.
    Unavailable(String),
    /// Anything else — network error, server error, auth failure.
    /// Raw git stderr is included verbatim so bug reports capture it.
    Other(String),
}

impl std::fmt::Display for LfsUnlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotOwner => f.write_str("this lock is owned by another user"),
            Self::Unavailable(reason) => write!(f, "LFS unavailable: {reason}"),
            Self::Other(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for LfsUnlockError {}

/// Enumerate every lock the remote is currently tracking for this
/// repo. Returns [`LfsListResult::Unavailable`] for the common "LFS
/// not configured" case so callers can render it inline; only genuine
/// errors (malformed JSON, unexpected failure mode) come back as
/// `Err`.
///
/// Implementation: `git lfs locks --json`. The JSON shape is a flat
/// array of objects like:
///
/// ```json
/// [{"id":"123","path":"a/b.psd","owner":{"name":"alice"},
///   "locked_at":"2024-06-01T12:00:00Z"}, ...]
/// ```
///
/// Fields we can't find are treated as missing rather than as parser
/// errors — servers have historically varied on which fields they
/// emit, and one missing `locked_at` shouldn't wipe out the whole
/// list. Entries without a `path` are dropped entirely because a
/// path-less lock isn't actionable from the UI.
pub fn list_locks(repo_path: &Path) -> Result<LfsListResult> {
    let out = super::cli::GitCommand::new(repo_path)
        .args(["lfs", "locks", "--json"])
        .run_raw()
        .context("spawn git lfs locks")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        return Ok(classify_list_error(&stderr));
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    // `git lfs locks --json` emits `null` instead of `[]` on some
    // older server builds when there are zero locks. Treat both as
    // "empty list".
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(LfsListResult::Ok { locks: Vec::new() });
    }
    Ok(LfsListResult::Ok {
        locks: parse_locks_json(trimmed)?,
    })
}

/// Acquire a lock on `rel_path` (relative to the repo root). Returns
/// the resulting [`LfsLock`] on success.
///
/// We reread the lock list after the successful `git lfs lock` call
/// rather than synthesising the returned struct from command-line
/// arguments, because the server is the source of truth for the lock
/// id and `locked_at` timestamp — and because another user might
/// have grabbed the lock between our decision to lock and the server
/// actually assigning it to us (in which case git-lfs itself returns
/// non-zero and we surface the stderr).
pub fn lock(repo_path: &Path, rel_path: &Path) -> Result<LfsLock> {
    let path_str = rel_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("lock: non-UTF-8 path {:?}", rel_path))?;
    let out = super::cli::GitCommand::new(repo_path)
        .args(["lfs", "lock", path_str])
        .run_raw()
        .context("spawn git lfs lock")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("git lfs lock failed: {}", stderr.trim());
    }

    // Refresh the lock list and find ours. We match on path rather
    // than trying to parse `git lfs lock`'s own stdout (which varies
    // across git-lfs versions — some emit "Locked <path>", others
    // emit a JSON blob when invoked with `--json`, which isn't
    // universally supported).
    match list_locks(repo_path)? {
        LfsListResult::Ok { locks } => locks
            .into_iter()
            .find(|lock| lock.path.as_path() == rel_path)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "git lfs lock succeeded but the new lock was not in the server list for {}",
                    rel_path.display()
                )
            }),
        LfsListResult::Unavailable { reason } => bail!(
            "git lfs lock succeeded but refresh reported LFS unavailable: {reason}"
        ),
    }
}

/// Release a lock by path. `force = true` passes `--force`, which
/// asks the server to drop a lock held by someone else (requires
/// admin permission on most servers — the server decides).
///
/// Returns `Err(LfsUnlockError::NotOwner)` specifically when git-lfs
/// reports the "owned by another user" condition, so the UI can
/// offer the "Force unlock" affordance cleanly. Other failures land
/// in `Other`.
pub fn unlock(repo_path: &Path, rel_path: &Path, force: bool) -> Result<(), LfsUnlockError> {
    let path_str = rel_path
        .to_str()
        .ok_or_else(|| LfsUnlockError::Other(format!("non-UTF-8 path {:?}", rel_path)))?;
    let mut args: Vec<&str> = vec!["lfs", "unlock"];
    if force {
        args.push("--force");
    }
    args.push(path_str);

    let out = super::cli::GitCommand::new(repo_path)
        .args(&args)
        .run_raw()
        .map_err(|e| LfsUnlockError::Other(format!("spawn git lfs unlock: {e:#}")))?;
    if out.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    Err(classify_unlock_error(&stderr))
}

/// Classify the stderr of a failed `git lfs locks` invocation into
/// either an `Unavailable` result (so the UI suppresses its error
/// toast) or propagate as a hard error. Kept as a pure function so
/// the tests can feed in synthetic stderr without touching `git`.
fn classify_list_error(stderr: &str) -> LfsListResult {
    let lower = stderr.to_ascii_lowercase();
    if stderr_indicates_lfs_not_installed(&lower) {
        return LfsListResult::Unavailable {
            reason: "Git LFS extension is not installed.".to_string(),
        };
    }
    if stderr_indicates_locks_api_unavailable(&lower) {
        return LfsListResult::Unavailable {
            reason: "Remote does not support the LFS locks API.".to_string(),
        };
    }
    if stderr_indicates_no_remote(&lower) {
        return LfsListResult::Unavailable {
            reason: "No LFS remote is configured for this repository.".to_string(),
        };
    }
    // Catch-all: fall back to Unavailable with the raw (trimmed)
    // message. The UI renders this inline; we prefer "known
    // inconvenience" over "scary error toast" for a feature that's
    // opt-in per repo.
    LfsListResult::Unavailable {
        reason: shorten_for_ui(stderr.trim()),
    }
}

fn classify_unlock_error(stderr: &str) -> LfsUnlockError {
    let lower = stderr.to_ascii_lowercase();
    if stderr_indicates_not_owner(&lower) {
        return LfsUnlockError::NotOwner;
    }
    if stderr_indicates_lfs_not_installed(&lower)
        || stderr_indicates_locks_api_unavailable(&lower)
        || stderr_indicates_no_remote(&lower)
    {
        return LfsUnlockError::Unavailable(shorten_for_ui(stderr.trim()));
    }
    LfsUnlockError::Other(stderr.trim().to_string())
}

fn stderr_indicates_lfs_not_installed(lower: &str) -> bool {
    // Two common shapes:
    //   * `git: 'lfs' is not a git command. See 'git --help'.`
    //   * `git-lfs not installed` (self-reported by some wrappers).
    lower.contains("'lfs' is not a git command") || lower.contains("git-lfs not installed")
}

fn stderr_indicates_locks_api_unavailable(lower: &str) -> bool {
    // git-lfs prints this when the server speaks the LFS media
    // protocol but not the locks sub-API. Phrasing has drifted
    // slightly between versions; we match on the stable core.
    lower.contains("locks api")
        && (lower.contains("unsupported")
            || lower.contains("not supported")
            || lower.contains("not available"))
}

fn stderr_indicates_no_remote(lower: &str) -> bool {
    // Happens when `.lfsconfig` isn't set and there's no default
    // remote that LFS can fall back to. git-lfs emits a variety of
    // "missing lfs url" phrasings across versions.
    lower.contains("missing protocol")
        || lower.contains("lfs.url")
        || lower.contains("no lfs api url")
}

fn stderr_indicates_not_owner(lower: &str) -> bool {
    // git-lfs rejects non-owner unlock with messages like:
    //   * "lock owned by ..."
    //   * "you are not the lock owner"
    //   * "forbidden" (rarely) — accompanied by "lock"
    lower.contains("not the lock owner")
        || lower.contains("owned by ")
        || (lower.contains("forbidden") && lower.contains("lock"))
}

/// Trim and cap stderr for UI display so a multi-kilobyte backtrace
/// from git-lfs doesn't blow up the sidebar layout.
fn shorten_for_ui(msg: &str) -> String {
    const MAX: usize = 180;
    let collapsed: String = msg.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > MAX {
        let mut out: String = collapsed.chars().take(MAX).collect();
        out.push('…');
        out
    } else {
        collapsed
    }
}

/// Parse the JSON array emitted by `git lfs locks --json`. Entries
/// missing a `path` are dropped — they're not actionable from the
/// UI and their presence is almost always a server-side bug. Other
/// missing fields default to their empty counterpart so a minor
/// server omission doesn't erase the whole list.
fn parse_locks_json(stdout: &str) -> Result<Vec<LfsLock>> {
    let parsed: Value = serde_json::from_str(stdout)
        .with_context(|| format!("parse git lfs locks JSON: {stdout}"))?;
    let arr = match parsed {
        Value::Array(items) => items,
        Value::Null => return Ok(Vec::new()),
        other => bail!(
            "git lfs locks --json: expected top-level array, got {}",
            value_type_name(&other)
        ),
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let obj = match item {
            Value::Object(o) => o,
            _ => continue,
        };
        // `path` is mandatory. Empty strings are treated as missing.
        let path_str = obj
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(path_str) = path_str else { continue };

        let id = obj
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // Owner is nested: `{"owner":{"name":"alice"}}`. Some older
        // servers emit it as a bare string instead — accept both.
        let owner = match obj.get("owner") {
            Some(Value::Object(owner_obj)) => owner_obj
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        let locked_at = obj
            .get("locked_at")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        out.push(LfsLock {
            id,
            path: PathBuf::from(path_str),
            owner,
            locked_at,
        });
    }
    Ok(out)
}

fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_array() {
        let locks = parse_locks_json("[]").unwrap();
        assert!(locks.is_empty());
    }

    #[test]
    fn parse_null_stdout() {
        let locks = parse_locks_json("null").unwrap();
        assert!(locks.is_empty());
    }

    #[test]
    fn parse_normal_list_with_multiple_entries() {
        let json = r#"[
            {"id":"1","path":"art/hero.psd",
             "owner":{"name":"alice"},
             "locked_at":"2024-06-01T12:00:00Z"},
            {"id":"2","path":"levels/intro.uasset",
             "owner":{"name":"bob"},
             "locked_at":"2024-06-02T09:30:00Z"}
        ]"#;
        let locks = parse_locks_json(json).unwrap();
        assert_eq!(locks.len(), 2);
        assert_eq!(locks[0].id, "1");
        assert_eq!(locks[0].path, PathBuf::from("art/hero.psd"));
        assert_eq!(locks[0].owner, "alice");
        assert_eq!(locks[0].locked_at.as_deref(), Some("2024-06-01T12:00:00Z"));
        assert_eq!(locks[1].owner, "bob");
    }

    #[test]
    fn parse_entry_with_missing_locked_at() {
        let json = r#"[{"id":"9","path":"a.bin","owner":{"name":"carol"}}]"#;
        let locks = parse_locks_json(json).unwrap();
        assert_eq!(locks.len(), 1);
        assert!(locks[0].locked_at.is_none());
    }

    #[test]
    fn parse_entry_with_missing_path_is_dropped() {
        // Path-less locks aren't actionable — drop them so the UI
        // doesn't render a lock row with no file to point at.
        let json = r#"[
            {"id":"10","owner":{"name":"dave"}},
            {"id":"11","path":"","owner":{"name":"erin"}},
            {"id":"12","path":"ok.txt","owner":{"name":"frank"}}
        ]"#;
        let locks = parse_locks_json(json).unwrap();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].path, PathBuf::from("ok.txt"));
    }

    #[test]
    fn parse_owner_as_bare_string() {
        // Older server implementations sent `"owner":"alice"` instead
        // of the nested object — accept both for compatibility.
        let json = r#"[{"id":"1","path":"x.bin","owner":"alice"}]"#;
        let locks = parse_locks_json(json).unwrap();
        assert_eq!(locks[0].owner, "alice");
    }

    #[test]
    fn parse_entry_with_missing_id_defaults_empty() {
        let json = r#"[{"path":"x.bin","owner":{"name":"zed"}}]"#;
        let locks = parse_locks_json(json).unwrap();
        assert_eq!(locks[0].id, "");
        assert_eq!(locks[0].owner, "zed");
    }

    #[test]
    fn parse_rejects_non_array_top_level() {
        let json = r#"{"id":"1","path":"x"}"#;
        assert!(parse_locks_json(json).is_err());
    }

    #[test]
    fn parse_rejects_invalid_json() {
        let json = "not json at all";
        assert!(parse_locks_json(json).is_err());
    }

    #[test]
    fn classify_list_error_not_installed() {
        let result = classify_list_error("git: 'lfs' is not a git command. See 'git --help'.");
        match result {
            LfsListResult::Unavailable { reason } => {
                assert!(reason.to_ascii_lowercase().contains("not installed"));
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn classify_list_error_no_lfs_url() {
        let result = classify_list_error("error: Missing protocol: lfs.url is not set");
        match result {
            LfsListResult::Unavailable { .. } => {}
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn classify_list_error_locks_api_not_supported() {
        let result =
            classify_list_error("ERROR: Server does not support the Locks API (not supported)");
        match result {
            LfsListResult::Unavailable { reason } => {
                assert!(reason.to_ascii_lowercase().contains("does not support"));
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn classify_list_error_unknown_falls_back_to_unavailable_with_reason() {
        // Unknown stderr lands in Unavailable (with the raw message
        // shortened for UI) rather than an error toast — the lock
        // panel is opt-in, we'd rather tuck an obscure failure under
        // an inline explanation than blast the user with a toast.
        let result = classify_list_error("something strange happened");
        match result {
            LfsListResult::Unavailable { reason } => {
                assert!(reason.contains("strange"));
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn classify_unlock_error_not_owner() {
        let err = classify_unlock_error("error: lock owned by alice");
        assert!(matches!(err, LfsUnlockError::NotOwner));
        let err = classify_unlock_error("you are not the lock owner");
        assert!(matches!(err, LfsUnlockError::NotOwner));
    }

    #[test]
    fn classify_unlock_error_unavailable() {
        let err = classify_unlock_error("git: 'lfs' is not a git command.");
        assert!(matches!(err, LfsUnlockError::Unavailable(_)));
    }

    #[test]
    fn classify_unlock_error_other() {
        let err = classify_unlock_error("connection refused: HTTP 502");
        assert!(matches!(err, LfsUnlockError::Other(_)));
    }

    #[test]
    fn shorten_collapses_and_caps() {
        let long = "a ".repeat(500);
        let out = shorten_for_ui(&long);
        assert!(out.chars().count() <= 181);
        // And it shouldn't contain runs of multiple spaces.
        assert!(!out.contains("  "));
    }
}
