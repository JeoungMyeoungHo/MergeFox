use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionRisk {
    Safe,
    Recoverable,
    Destructive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewLine {
    pub severity: PreviewSeverity,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewSeverity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionPreview {
    pub label: String,
    pub effect: String,
    pub risk: ActionRisk,
    pub confirmation_required: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<PreviewLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionRequest {
    CopySha {
        oid: String,
    },
    CopyShortSha {
        oid: String,
    },
    Checkout {
        oid: String,
    },
    CheckoutBranch {
        branch: String,
    },
    CreateBranch {
        at: String,
        name: Option<String>,
    },
    CreateTag {
        at: String,
        annotated: bool,
        name: Option<String>,
    },
    CherryPick {
        commits: Vec<String>,
    },
    Revert {
        oid: String,
    },
    Reset {
        branch: String,
        mode: String,
        target: String,
    },
    AmendMessage,
    DropCommit {
        oid: String,
    },
    MoveCommitUp {
        oid: String,
    },
    MoveCommitDown {
        oid: String,
    },
    Pull {
        branch: String,
        remote: Option<String>,
    },
    Push {
        branch: String,
        force: bool,
        remote: Option<String>,
    },
    SetUpstream {
        branch: String,
    },
    RenameBranch {
        from: String,
    },
    DeleteBranch {
        name: String,
        is_remote: bool,
    },
    CreateWorktree {
        oid: String,
    },
    StashPush,
    StashPop {
        index: usize,
    },
    StashApply {
        index: usize,
    },
    StashDrop {
        index: usize,
        message: Option<String>,
    },
}

pub fn preview(repo_path: &Path, req: ActionRequest) -> Result<ActionPreview> {
    let preview = match req {
        ActionRequest::CopySha { oid } => safe(
            format!("Copy SHA {oid}"),
            "Copies the full commit id to the clipboard only.",
        ),
        ActionRequest::CopyShortSha { oid } => safe(
            format!("Copy short SHA {}", short(&oid)),
            "Copies the abbreviated commit id to the clipboard only.",
        ),
        ActionRequest::Checkout { oid } => recoverable(
            format!("Checkout {}", short(&oid)),
            "Moves HEAD to the selected commit in detached mode. Dirty changes are auto-stashed first.",
        ),
        ActionRequest::CheckoutBranch { branch } => recoverable(
            format!("Checkout branch {branch}"),
            "Switches HEAD to the selected branch. Dirty changes are auto-stashed first.",
        ),
        ActionRequest::CreateBranch { at, name } => safe(
            format!(
                "Create branch {} at {}",
                name.unwrap_or_else(|| "<new>".into()),
                short(&at)
            ),
            "Creates a new branch ref without rewriting existing history.",
        ),
        ActionRequest::CreateTag {
            at,
            annotated,
            name,
        } => safe(
            format!(
                "Create {}tag {} at {}",
                if annotated { "annotated " } else { "" },
                name.unwrap_or_else(|| "<new>".into()),
                short(&at)
            ),
            "Adds a new tag ref without modifying commits.",
        ),
        ActionRequest::CherryPick { commits } => recoverable(
            format!("Cherry-pick {} commit(s)", commits.len()),
            "Replays the selected commit(s) onto the current branch. Conflicts may pause the operation mid-way.",
        ),
        ActionRequest::Revert { oid } => recoverable(
            format!("Revert {}", short(&oid)),
            "Creates a new commit that inverses the selected commit. Existing history stays intact.",
        ),
        ActionRequest::Reset {
            branch,
            mode,
            target,
        } => preview_reset(repo_path, &branch, &mode, &target)?,
        ActionRequest::AmendMessage => recoverable(
            "Amend HEAD message".to_string(),
            "Rewrites the current HEAD commit with a new message (and optional author change).",
        ),
        ActionRequest::DropCommit { oid } => destructive(
            format!("Drop {}", short(&oid)),
            "Removes the selected commit by rewriting branch history.",
            info_lines(crate::preflight::drop_commit(repo_path, parse_oid(&oid)?)),
            true,
        ),
        ActionRequest::MoveCommitUp { oid } => destructive(
            format!("Move {} up", short(&oid)),
            "Reorders commits by rewriting history around the selected commit.",
            vec![PreviewLine {
                severity: PreviewSeverity::Warning,
                text: "Commit order changes rewrite descendant commits and may create conflicts."
                    .into(),
            }],
            true,
        ),
        ActionRequest::MoveCommitDown { oid } => destructive(
            format!("Move {} down", short(&oid)),
            "Reorders commits by rewriting history around the selected commit.",
            vec![PreviewLine {
                severity: PreviewSeverity::Warning,
                text: format!(
                    "Moving {} later in history rewrites descendant commits.",
                    short(&oid)
                ),
            }],
            true,
        ),
        ActionRequest::Pull { branch, remote } => recoverable(
            format!(
                "Pull {} from {}",
                branch,
                remote.unwrap_or_else(|| default_remote(repo_path))
            ),
            "Fetches and integrates upstream changes into the current branch.",
        ),
        ActionRequest::Push {
            branch,
            force,
            remote,
        } => preview_push(repo_path, &branch, force, remote.as_deref())?,
        ActionRequest::SetUpstream { branch } => safe(
            format!("Set upstream for {branch}"),
            "Changes branch tracking metadata only. Commits and refs stay untouched.",
        ),
        ActionRequest::RenameBranch { from } => recoverable(
            format!("Rename branch {from}"),
            "Renames the branch ref and updates local tracking configuration.",
        ),
        ActionRequest::DeleteBranch { name, is_remote } => {
            let lines = info_lines(crate::preflight::delete_branch(repo_path, &name, is_remote));
            let risk = if is_remote {
                ActionRisk::Recoverable
            } else {
                ActionRisk::Destructive
            };
            ActionPreview {
                label: format!(
                    "Delete {}branch {name}",
                    if is_remote { "remote-tracking " } else { "" }
                ),
                effect: if is_remote {
                    "Removes the local remote-tracking ref only.".into()
                } else {
                    "Deletes the local branch ref. Unique commits become reachable only via reflog."
                        .into()
                },
                risk,
                confirmation_required: !is_remote,
                lines,
            }
        }
        ActionRequest::CreateWorktree { oid } => recoverable(
            format!("Create worktree from {}", short(&oid)),
            "Creates a new working tree rooted at the selected commit without changing the current checkout.",
        ),
        ActionRequest::StashPush => safe(
            "Create stash".to_string(),
            "Saves the current working tree and index into a new stash entry.",
        ),
        ActionRequest::StashPop { index } => recoverable(
            format!("Pop stash@{{{index}}}"),
            "Applies the stash and removes it from the stash list if the apply succeeds.",
        ),
        ActionRequest::StashApply { index } => recoverable(
            format!("Apply stash@{{{index}}}"),
            "Applies the stash without dropping the original stash entry.",
        ),
        ActionRequest::StashDrop { index, message } => destructive(
            format!("Drop stash@{{{index}}}"),
            format!(
                "Removes the stash entry{}.",
                message
                    .as_deref()
                    .filter(|m| !m.trim().is_empty())
                    .map(|m| format!(" ({m})"))
                    .unwrap_or_default()
            ),
            vec![PreviewLine {
                severity: PreviewSeverity::Warning,
                text: "Dropped stash entries are only recoverable for a short time via reflog."
                    .into(),
            }],
            true,
        ),
    };
    Ok(preview)
}

fn preview_reset(
    repo_path: &Path,
    branch: &str,
    mode: &str,
    target: &str,
) -> Result<ActionPreview> {
    let mode = mode.trim().to_ascii_lowercase();
    let oid = parse_oid(target)?;
    let preview = match mode.as_str() {
        "hard" => destructive(
            format!("Hard reset {branch} -> {}", short(target)),
            "Moves the branch tip and resets the working tree/index to match the target commit.",
            info_lines(crate::preflight::hard_reset(repo_path, branch, oid)),
            true,
        ),
        "mixed" => recoverable(
            format!("Mixed reset {branch} -> {}", short(target)),
            "Moves the branch tip and resets the index, but leaves working-tree files in place.",
        ),
        "soft" => recoverable(
            format!("Soft reset {branch} -> {}", short(target)),
            "Moves the branch tip only. Index and working tree stay intact.",
        ),
        other => anyhow::bail!("unsupported reset mode `{other}`"),
    };
    Ok(preview)
}

fn preview_push(
    repo_path: &Path,
    branch: &str,
    force: bool,
    remote: Option<&str>,
) -> Result<ActionPreview> {
    let remote = remote
        .map(str::to_string)
        .unwrap_or_else(|| default_remote(repo_path));
    let preview = if force {
        destructive(
            format!("Force-push {branch} -> {remote}"),
            "Overwrites the remote branch with the local branch state.",
            info_lines(crate::preflight::force_push(repo_path, &remote, branch)),
            true,
        )
    } else {
        recoverable(
            format!("Push {branch} -> {remote}"),
            "Uploads local commits to the remote branch without rewriting remote history.",
        )
    };
    Ok(preview)
}

fn default_remote(repo_path: &Path) -> String {
    if let Some(remote) = crate::config::Config::load()
        .repo_settings_for(repo_path)
        .default_remote
    {
        return remote;
    }
    "origin".into()
}

fn safe(label: impl Into<String>, effect: impl Into<String>) -> ActionPreview {
    ActionPreview {
        label: label.into(),
        effect: effect.into(),
        risk: ActionRisk::Safe,
        confirmation_required: false,
        lines: Vec::new(),
    }
}

fn recoverable(label: impl Into<String>, effect: impl Into<String>) -> ActionPreview {
    ActionPreview {
        label: label.into(),
        effect: effect.into(),
        risk: ActionRisk::Recoverable,
        confirmation_required: false,
        lines: Vec::new(),
    }
}

fn destructive(
    label: impl Into<String>,
    effect: impl Into<String>,
    lines: Vec<PreviewLine>,
    confirmation_required: bool,
) -> ActionPreview {
    ActionPreview {
        label: label.into(),
        effect: effect.into(),
        risk: ActionRisk::Destructive,
        confirmation_required,
        lines,
    }
}

fn info_lines(info: crate::preflight::PreflightInfo) -> Vec<PreviewLine> {
    info.lines
        .into_iter()
        .map(|line| PreviewLine {
            severity: match line.severity {
                crate::preflight::Severity::Info => PreviewSeverity::Info,
                crate::preflight::Severity::Warning => PreviewSeverity::Warning,
                crate::preflight::Severity::Critical => PreviewSeverity::Critical,
            },
            text: line.text,
        })
        .collect()
}

fn parse_oid(raw: &str) -> Result<gix::ObjectId> {
    raw.parse()
        .with_context(|| format!("invalid object id `{raw}`"))
}

fn short(raw: &str) -> String {
    raw.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::{preview, ActionRequest, ActionRisk};

    #[test]
    fn safe_copy_preview_needs_no_confirmation() {
        let preview = preview(
            std::path::Path::new("."),
            ActionRequest::CopyShortSha {
                oid: "0123456789abcdef".into(),
            },
        )
        .unwrap();
        assert_eq!(preview.risk, ActionRisk::Safe);
        assert!(!preview.confirmation_required);
    }

    #[test]
    fn stash_drop_is_destructive() {
        let preview = preview(
            std::path::Path::new("."),
            ActionRequest::StashDrop {
                index: 3,
                message: Some("wip".into()),
            },
        )
        .unwrap();
        assert_eq!(preview.risk, ActionRisk::Destructive);
        assert!(preview.confirmation_required);
    }
}
