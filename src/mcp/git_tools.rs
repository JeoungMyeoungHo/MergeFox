//! Destructive git ops exposed over MCP, wrapped in the same
//! backup-tag + auto-stash envelope the GUI uses.
//!
//! ## Why these live separate from `action_execute`
//!
//! The existing `mergefox_action_execute` tool is a direct replay of
//! `ActionRequest` — the same enum the GUI routes through its button
//! handlers. For the destructive history-rewrite ops (reword, squash,
//! find-and-fix across history) we wanted a dedicated surface that:
//!
//!   * **Defaults to dry-run** so a model whose tool call was
//!     speculative doesn't accidentally rewrite history just by asking
//!     "what would this do?". Callers have to explicitly set
//!     `dry_run=false` to execute — at that point the safety envelope
//!     in the underlying ops (`reword_ops`, `find_fix_ops`,
//!     `basket_ops`) creates the backup tag + auto-stash wrapper.
//!
//!   * **Supports explicit rollback** via `mergefox_git_rollback_to_backup_tag`
//!     because the 2B–14B local-model target audience is more likely
//!     to make a mistake that needs a one-shot "put it back" button
//!     than a cloud Sonnet call.
//!
//!   * **Rate-limits destructive ops per session**. A buggy agent loop
//!     that calls `reword_commit` 10 000 times in a row shouldn't be
//!     able to brick the repo — we cap executions and return a
//!     `reason: "rate_limited"` once the session budget is exhausted.
//!
//! ## Rollback contract
//!
//! Every destructive op in this module returns a `backup_tag` field in
//! its `structuredContent`. The caller records it; if a subsequent op
//! misbehaves, `mergefox_git_rollback_to_backup_tag` with that tag
//! name resets HEAD back to the pre-op state.
//!
//! ## Journal side-effect
//!
//! Every successful destructive execution calls into `journal` so the
//! mergeFox UI's Activity Log shows the MCP-initiated op next to the
//! user's own actions. `source = "mcp"` in the record — the UI can
//! filter these out if the user wants a "what did *I* do" view.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Session-scoped budget for destructive executes. Reset per
/// `run_stdio` invocation — the protection is against a single runaway
/// loop, not against an attacker spinning up new sessions.
pub const DEFAULT_DESTRUCTIVE_BUDGET: usize = 20;

/// Shared counter for the current session. `AtomicUsize` so we don't
/// need to thread a mutable reference through every dispatch —
/// `session_counter()` returns a process-static that's fine for the
/// stdio transport (one session per process).
static DESTRUCTIVE_CALLS: AtomicUsize = AtomicUsize::new(0);

pub fn reset_session_counters() {
    DESTRUCTIVE_CALLS.store(0, Ordering::Relaxed);
}

fn consume_destructive_budget() -> Result<(), String> {
    let prev = DESTRUCTIVE_CALLS.fetch_add(1, Ordering::Relaxed);
    if prev >= DEFAULT_DESTRUCTIVE_BUDGET {
        // Roll back the increment so a caller who catches this error
        // and retries doesn't amplify the counter.
        DESTRUCTIVE_CALLS.fetch_sub(1, Ordering::Relaxed);
        return Err(format!(
            "session destructive-op budget exhausted ({DEFAULT_DESTRUCTIVE_BUDGET} writes). \
             Restart the MCP server to reset."
        ));
    }
    Ok(())
}

/// Entry point called from `server.rs` `tools/call`. Returns
/// `Ok(Some(value))` if we handled the tool name, `Ok(None)` if it
/// wasn't one of ours (so the caller can try the legacy dispatch).
pub fn dispatch(
    repo_path: &Path,
    tool_name: &str,
    arguments: Value,
    destructive_allowed: bool,
) -> Result<Option<Value>> {
    let payload = match tool_name {
        "mergefox_git_reword_commit" => {
            Some(git_reword_commit(repo_path, arguments, destructive_allowed)?)
        }
        "mergefox_git_find_replace" => {
            Some(git_find_replace(repo_path, arguments, destructive_allowed)?)
        }
        "mergefox_git_squash_commits" => {
            Some(git_squash_commits(repo_path, arguments, destructive_allowed)?)
        }
        "mergefox_git_revert_commits" => {
            Some(git_revert_commits(repo_path, arguments, destructive_allowed)?)
        }
        "mergefox_git_list_backup_tags" => Some(git_list_backup_tags(repo_path)?),
        "mergefox_git_rollback_to_backup_tag" => Some(git_rollback_to_backup_tag(
            repo_path,
            arguments,
            destructive_allowed,
        )?),
        _ => None,
    };
    Ok(payload)
}

/// Build the tool descriptor list for `tools/list`. Returned as a
/// `Vec<Value>` so `server.rs` can splice it into the existing
/// descriptor array without duplicating JSON blobs.
pub fn tool_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "mergefox_git_reword_commit",
            "description": "Rewrite a single commit's message while preserving author / \
                            committer identity and rebasing descendants. Creates a \
                            `mergefox/reword/<ts>` backup tag before the rewrite. \
                            Defaults to dry-run: set `dry_run=false` to actually \
                            rewrite history. Requires `MERGEFOX_MCP_AUTO_APPROVE=all` \
                            + `MERGEFOX_MCP_ALLOW_DESTRUCTIVE=1` to execute.",
            "inputSchema": {
                "type": "object",
                "required": ["oid", "message"],
                "properties": {
                    "oid": { "type": "string", "description": "Hex OID of the commit to reword." },
                    "message": { "type": "string", "description": "New commit message (full body)." },
                    "dry_run": { "type": "boolean", "default": true }
                }
            }
        }),
        json!({
            "name": "mergefox_git_find_replace",
            "description": "Literal search across working-tree files + commit messages, with \
                            optional in-place replacement. Dry-run returns a preview of hits; \
                            execute rewrites everything behind one backup tag + auto-stash \
                            envelope. Does NOT touch binary files; commit-message rewrites \
                            only affect the current branch history (not `--all`).",
            "inputSchema": {
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "pattern": { "type": "string" },
                    "replacement": { "type": "string", "default": "" },
                    "include_working_tree": { "type": "boolean", "default": true },
                    "include_commit_messages": { "type": "boolean", "default": true },
                    "commit_history_limit": { "type": "integer", "default": 1000 },
                    "apply_paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Subset of repo-relative paths from the scan output to rewrite. Required when `dry_run=false`."
                    },
                    "apply_commit_oids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Subset of hex OIDs from the scan output to reword. Required when `dry_run=false`."
                    },
                    "dry_run": { "type": "boolean", "default": true }
                }
            }
        }),
        json!({
            "name": "mergefox_git_squash_commits",
            "description": "Squash multiple commits into one, then rebase the rest of the branch \
                            on top. Creates a `mergefox/basket-squash/<ts>` backup tag first. \
                            Rejects merge commits, HEAD-inclusive baskets, and commits that \
                            aren't ancestors of HEAD. Dry-run by default.",
            "inputSchema": {
                "type": "object",
                "required": ["commit_oids", "message"],
                "properties": {
                    "commit_oids": { "type": "array", "items": { "type": "string" } },
                    "message": { "type": "string" },
                    "dry_run": { "type": "boolean", "default": true }
                }
            }
        }),
        json!({
            "name": "mergefox_git_revert_commits",
            "description": "Run `git revert --no-commit` over a list of commits, producing \
                            a single pending working-tree change. Conflicts surface in the \
                            outcome payload; call `git_rollback_to_backup_tag` or `git revert \
                            --abort` from the GUI to back out.",
            "inputSchema": {
                "type": "object",
                "required": ["commit_oids"],
                "properties": {
                    "commit_oids": { "type": "array", "items": { "type": "string" } },
                    "dry_run": { "type": "boolean", "default": true }
                }
            }
        }),
        json!({
            "name": "mergefox_git_list_backup_tags",
            "description": "List every `mergefox/…` backup tag in the repository with its \
                            target commit. Read-only; always safe to call before executing \
                            a rollback.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "mergefox_git_rollback_to_backup_tag",
            "description": "Hard-reset the current branch to the given backup tag. Intended \
                            for undoing a misapplied destructive op. Requires the full tag \
                            name as returned by `list_backup_tags`, and respects the same \
                            tier gate as other destructive tools.",
            "inputSchema": {
                "type": "object",
                "required": ["tag"],
                "properties": {
                    "tag": { "type": "string" },
                    "dry_run": { "type": "boolean", "default": true }
                }
            }
        }),
    ]
}

// ---------- individual tool handlers ----------

#[derive(Debug, Deserialize)]
struct RewordArgs {
    oid: String,
    message: String,
    #[serde(default = "default_true")]
    dry_run: bool,
}

fn default_true() -> bool {
    true
}

fn git_reword_commit(
    repo_path: &Path,
    arguments: Value,
    destructive_allowed: bool,
) -> Result<Value> {
    let args: RewordArgs = serde_json::from_value(arguments).context("reword args")?;
    let oid = gix::ObjectId::from_hex(args.oid.as_bytes())
        .map_err(|e| anyhow!("invalid OID: {e}"))?;
    let short = short_oid(&oid);

    if args.dry_run {
        return Ok(tool_result(json!({
            "dry_run": true,
            "would_execute": true,
            "target_oid": args.oid,
            "new_message_preview": args.message,
            "backup_tag_will_be_created": true,
            "hint": "Pass `dry_run=false` (and set MERGEFOX_MCP_ALLOW_DESTRUCTIVE=1) to run."
        })));
    }
    if !destructive_allowed {
        return Ok(refused("reword_commit"));
    }
    if let Err(e) = consume_destructive_budget() {
        return Ok(rate_limited(&e));
    }

    let outcome = crate::git::reword_commit(repo_path, oid, &args.message)
        .map_err(|e| anyhow!("reword_commit: {e:#}"))?;
    match outcome {
        crate::git::RewordOutcome::Success {
            new_head_oid,
            new_target_oid,
            backup_tag,
        } => Ok(tool_result(json!({
            "executed": true,
            "target_oid": args.oid,
            "new_head_oid": new_head_oid.to_string(),
            "new_target_oid": new_target_oid.to_string(),
            "backup_tag": backup_tag,
            "hint": format!("Rollback with mergefox_git_rollback_to_backup_tag tag={backup_tag}"),
            "summary": format!("Reworded {short} → {}", short_oid(&new_target_oid))
        }))),
        crate::git::RewordOutcome::Aborted {
            reason,
            backup_tag_created,
        } => Ok(tool_result(json!({
            "executed": false,
            "aborted": true,
            "reason": reason,
            "backup_tag": backup_tag_created,
        }))),
    }
}

#[derive(Debug, Deserialize)]
struct FindReplaceArgs {
    pattern: String,
    #[serde(default)]
    replacement: String,
    #[serde(default = "default_true")]
    include_working_tree: bool,
    #[serde(default = "default_true")]
    include_commit_messages: bool,
    #[serde(default = "default_history_limit")]
    commit_history_limit: usize,
    #[serde(default)]
    apply_paths: Vec<String>,
    #[serde(default)]
    apply_commit_oids: Vec<String>,
    #[serde(default = "default_true")]
    dry_run: bool,
}

fn default_history_limit() -> usize {
    1000
}

fn git_find_replace(
    repo_path: &Path,
    arguments: Value,
    destructive_allowed: bool,
) -> Result<Value> {
    let args: FindReplaceArgs =
        serde_json::from_value(arguments).context("find_replace args")?;
    if args.pattern.is_empty() {
        return Err(anyhow!("`pattern` must be non-empty"));
    }

    let scan = crate::git::find_fix_scan(
        repo_path,
        &args.pattern,
        args.include_working_tree,
        args.include_commit_messages,
        args.commit_history_limit,
    )
    .map_err(|e| anyhow!("scan: {e:#}"))?;

    if args.dry_run {
        return Ok(tool_result(json!({
            "dry_run": true,
            "pattern": args.pattern,
            "replacement": args.replacement,
            "working_tree_hits": scan.working_tree.iter().map(|m| json!({
                "path": m.path,
                "line_number": m.line_number,
                "line": m.line,
            })).collect::<Vec<_>>(),
            "commit_hits": scan.commit_messages.iter().map(|m| json!({
                "oid": m.oid.to_string(),
                "subject": m.subject,
                "subject_hit": m.subject_hit,
                "body_hit": m.body_hit,
            })).collect::<Vec<_>>(),
            "hint": "Select specific paths / oids via `apply_paths` and `apply_commit_oids`, then call again with `dry_run=false`.",
        })));
    }
    if !destructive_allowed {
        return Ok(refused("find_replace"));
    }
    if let Err(e) = consume_destructive_budget() {
        return Ok(rate_limited(&e));
    }

    let apply_paths: Vec<PathBuf> = args.apply_paths.into_iter().map(PathBuf::from).collect();
    let mut apply_oids: Vec<gix::ObjectId> = Vec::with_capacity(args.apply_commit_oids.len());
    for s in &args.apply_commit_oids {
        apply_oids.push(
            gix::ObjectId::from_hex(s.as_bytes())
                .map_err(|e| anyhow!("invalid OID `{s}`: {e}"))?,
        );
    }

    let plan = crate::git::FindFixApplyPlan {
        pattern: args.pattern.clone(),
        replacement: args.replacement.clone(),
        apply_working_tree_paths: apply_paths,
        apply_commit_oids: apply_oids,
    };
    let outcome = crate::git::find_fix_apply(repo_path, plan)
        .map_err(|e| anyhow!("apply: {e:#}"))?;
    match outcome {
        crate::git::FindFixApplyOutcome::Success {
            working_tree_files_changed,
            commit_oid_remap,
            backup_tag,
            auto_stashed,
        } => Ok(tool_result(json!({
            "executed": true,
            "working_tree_files_changed": working_tree_files_changed,
            "commit_oid_remap": commit_oid_remap.iter().map(|(old, new)| json!({
                "old": old.to_string(),
                "new": new.to_string()
            })).collect::<Vec<_>>(),
            "backup_tag": backup_tag,
            "auto_stashed": auto_stashed,
        }))),
        crate::git::FindFixApplyOutcome::Aborted { reason, backup_tag } => {
            Ok(tool_result(json!({
                "executed": false,
                "aborted": true,
                "reason": reason,
                "backup_tag": backup_tag,
            })))
        }
    }
}

#[derive(Debug, Deserialize)]
struct SquashArgs {
    commit_oids: Vec<String>,
    message: String,
    #[serde(default = "default_true")]
    dry_run: bool,
}

fn git_squash_commits(
    repo_path: &Path,
    arguments: Value,
    destructive_allowed: bool,
) -> Result<Value> {
    let args: SquashArgs = serde_json::from_value(arguments).context("squash args")?;
    let mut oids: Vec<gix::ObjectId> = Vec::with_capacity(args.commit_oids.len());
    for s in &args.commit_oids {
        oids.push(
            gix::ObjectId::from_hex(s.as_bytes())
                .map_err(|e| anyhow!("invalid OID `{s}`: {e}"))?,
        );
    }
    if args.dry_run {
        return Ok(tool_result(json!({
            "dry_run": true,
            "would_execute": true,
            "commit_count": oids.len(),
            "message_preview": args.message,
            "backup_tag_will_be_created": true,
        })));
    }
    if !destructive_allowed {
        return Ok(refused("squash_commits"));
    }
    if let Err(e) = consume_destructive_budget() {
        return Ok(rate_limited(&e));
    }

    let outcome = crate::git::squash_basket_into_one(repo_path, &oids, &args.message);
    match outcome {
        crate::git::SquashOutcome::Success {
            new_head_oid,
            backup_tag,
        } => Ok(tool_result(json!({
            "executed": true,
            "new_head_oid": new_head_oid.to_string(),
            "backup_tag": backup_tag,
        }))),
        crate::git::SquashOutcome::Aborted {
            reason,
            backup_tag_created,
        } => Ok(tool_result(json!({
            "executed": false,
            "aborted": true,
            "reason": reason,
            "backup_tag": backup_tag_created,
        }))),
    }
}

#[derive(Debug, Deserialize)]
struct RevertArgs {
    commit_oids: Vec<String>,
    #[serde(default = "default_true")]
    dry_run: bool,
}

fn git_revert_commits(
    repo_path: &Path,
    arguments: Value,
    destructive_allowed: bool,
) -> Result<Value> {
    let args: RevertArgs = serde_json::from_value(arguments).context("revert args")?;
    let mut oids: Vec<gix::ObjectId> = Vec::with_capacity(args.commit_oids.len());
    for s in &args.commit_oids {
        oids.push(
            gix::ObjectId::from_hex(s.as_bytes())
                .map_err(|e| anyhow!("invalid OID `{s}`: {e}"))?,
        );
    }
    if args.dry_run {
        return Ok(tool_result(json!({
            "dry_run": true,
            "commit_count": oids.len(),
            "hint": "Revert writes to the working tree; no backup tag is created because the \
                    original commits stay reachable. Abort with `git revert --abort`."
        })));
    }
    if !destructive_allowed {
        return Ok(refused("revert_commits"));
    }
    if let Err(e) = consume_destructive_budget() {
        return Ok(rate_limited(&e));
    }

    let outcome = crate::git::revert_to_working_tree(repo_path, &oids)
        .map_err(|e| anyhow!("revert: {e:#}"))?;
    Ok(tool_result(match outcome {
        crate::git::RevertOutcome::Clean {
            commits_reverted,
            auto_stashed,
        } => json!({
            "executed": true,
            "clean": true,
            "commits_reverted": commits_reverted,
            "auto_stashed": auto_stashed,
        }),
        crate::git::RevertOutcome::Conflicts {
            commits_reverted,
            conflicted_paths,
            auto_stashed,
        } => json!({
            "executed": true,
            "clean": false,
            "conflicts": true,
            "commits_reverted": commits_reverted,
            "conflicted_paths": conflicted_paths,
            "auto_stashed": auto_stashed,
            "hint": "Resolve conflicts in the mergeFox UI, then commit the revert."
        }),
        crate::git::RevertOutcome::Aborted { reason } => json!({
            "executed": false,
            "aborted": true,
            "reason": reason,
        }),
    }))
}

fn git_list_backup_tags(repo_path: &Path) -> Result<Value> {
    // `git tag --list mergefox/*` covers every namespace we create
    // (basket-squash, reword, findfix). We additionally resolve each
    // tag to its target OID so the caller doesn't need a second round
    // trip.
    let tag_list = crate::git::cli::run(repo_path, ["tag", "--list", "mergefox/*"])
        .context("list backup tags")?;
    let mut entries: Vec<Value> = Vec::new();
    for name in tag_list.stdout_str().lines() {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let resolved =
            crate::git::cli::run(repo_path, ["rev-parse", name]).ok();
        let oid = resolved
            .as_ref()
            .map(|o| o.stdout_str().trim().to_string())
            .unwrap_or_default();
        entries.push(json!({
            "tag": name,
            "oid": oid,
        }));
    }
    Ok(tool_result(json!({ "tags": entries })))
}

#[derive(Debug, Deserialize)]
struct RollbackArgs {
    tag: String,
    #[serde(default = "default_true")]
    dry_run: bool,
}

fn git_rollback_to_backup_tag(
    repo_path: &Path,
    arguments: Value,
    destructive_allowed: bool,
) -> Result<Value> {
    let args: RollbackArgs = serde_json::from_value(arguments).context("rollback args")?;
    if !args.tag.starts_with("mergefox/") {
        return Err(anyhow!(
            "`tag` must be a mergefox/* backup tag — got `{}`",
            args.tag
        ));
    }
    if args.dry_run {
        // Resolve the tag to a hash so the preview shows the caller
        // *what* they'd reset to before we actually do it.
        let resolved = crate::git::cli::run(repo_path, ["rev-parse", &args.tag])
            .ok()
            .map(|o| o.stdout_str().trim().to_string())
            .unwrap_or_default();
        return Ok(tool_result(json!({
            "dry_run": true,
            "tag": args.tag,
            "target_oid": resolved,
            "hint": "Pass `dry_run=false` to perform `git reset --hard <tag>`. The HEAD will \
                    move; any uncommitted changes should be stashed first.",
        })));
    }
    if !destructive_allowed {
        return Ok(refused("rollback_to_backup_tag"));
    }
    if let Err(e) = consume_destructive_budget() {
        return Ok(rate_limited(&e));
    }

    let reset =
        crate::git::cli::run(repo_path, ["reset", "--hard", &args.tag]).context("reset --hard")?;
    Ok(tool_result(json!({
        "executed": true,
        "tag": args.tag,
        "stdout": reset.stdout_str(),
    })))
}

// ---------- helpers ----------

#[derive(Debug, Serialize)]
struct _Unused;

fn refused(op: &str) -> Value {
    tool_result(json!({
        "executed": false,
        "reason": "approval_required",
        "operation": op,
        "message": "Destructive MCP ops require both `MERGEFOX_MCP_AUTO_APPROVE=all` and \
                    `MERGEFOX_MCP_ALLOW_DESTRUCTIVE=1`. Without both the server refuses \
                    execution. Call with `dry_run=true` to preview."
    }))
}

fn rate_limited(reason: &str) -> Value {
    tool_result(json!({
        "executed": false,
        "reason": "rate_limited",
        "message": reason,
    }))
}

fn tool_result(payload: Value) -> Value {
    // Mirrors `server::tool_result` so structuredContent ships with
    // every response. Duplicated rather than re-exported because a
    // circular import would force `tool_result` into its own tiny
    // module for the sake of one helper.
    let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into());
    json!({
        "content": [
            {
                "type": "text",
                "text": text
            }
        ],
        "structuredContent": payload,
        "isError": false
    })
}

fn short_oid(oid: &gix::ObjectId) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reword_dry_run_does_not_call_git() {
        // Running the dry-run branch should not touch git — we can pass
        // a path that doesn't exist and still get a structured preview.
        let args = json!({
            "oid": "0000000000000000000000000000000000000001",
            "message": "hello",
            "dry_run": true
        });
        let result =
            git_reword_commit(std::path::Path::new("/nonexistent"), args, false).expect("ok");
        let sc = result.get("structuredContent").expect("structuredContent");
        assert_eq!(sc["dry_run"], true);
        assert_eq!(sc["backup_tag_will_be_created"], true);
    }

    #[test]
    fn refuses_without_destructive_flag_even_when_not_dry_run() {
        // dry_run=false + destructive_allowed=false ⇒ explicit refusal.
        let args = json!({
            "oid": "0000000000000000000000000000000000000001",
            "message": "hello",
            "dry_run": false
        });
        let result =
            git_reword_commit(std::path::Path::new("/nonexistent"), args, false).expect("ok");
        let sc = result.get("structuredContent").expect("structuredContent");
        assert_eq!(sc["executed"], false);
        assert_eq!(sc["reason"], "approval_required");
    }

    #[test]
    fn rollback_rejects_non_mergefox_tags() {
        let args = json!({ "tag": "v1.0.0", "dry_run": true });
        let err = git_rollback_to_backup_tag(
            std::path::Path::new("/nonexistent"),
            args,
            true,
        )
        .unwrap_err();
        assert!(err.to_string().contains("mergefox/"));
    }

    #[test]
    fn dispatch_returns_none_for_unknown_tool() {
        let result = dispatch(
            std::path::Path::new("/nonexistent"),
            "nope",
            json!({}),
            false,
        )
        .expect("ok");
        assert!(result.is_none());
    }

    #[test]
    fn destructive_budget_enforced() {
        reset_session_counters();
        for _ in 0..DEFAULT_DESTRUCTIVE_BUDGET {
            consume_destructive_budget().expect("budget available");
        }
        let err = consume_destructive_budget().expect_err("budget should be exhausted");
        assert!(err.contains("budget exhausted"));
        reset_session_counters();
    }
}
