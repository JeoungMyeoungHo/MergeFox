//! Journal entry types — serialized as JSON lines.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub type EntryId = u64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: EntryId,
    pub timestamp_unix: i64,
    pub operation: Operation,
    pub before: RepoSnapshot,
    pub after: RepoSnapshot,
    pub source: OpSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum OpSource {
    Ui,
    Mcp { agent: String },
    External,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Operation {
    Commit {
        message: String,
        amended: bool,
    },
    Checkout {
        from: Option<String>,
        to: String,
    },
    CreateBranch {
        name: String,
        at: String,
    },
    DeleteBranch {
        name: String,
        tip: String,
    },
    Merge {
        into: String,
        from: String,
    },
    Rebase {
        branch: String,
        onto: String,
    },
    Reset {
        branch: String,
        mode: String,
        target: String,
    },
    StashPush {
        message: String,
    },
    StashPop {
        stash_oid: String,
    },
    CherryPick {
        commits: Vec<String>,
    },
    Revert {
        commits: Vec<String>,
    },
    ForcePush {
        remote: String,
        branch: String,
        from_sha: String,
        to_sha: String,
    },
    /// Free-form label for operations we haven't modelled yet —
    /// keeps the journal recording even when we stub actions.
    Raw {
        label: String,
    },
}

impl Operation {
    /// Short human-readable label for HUD / history UI.
    pub fn label(&self) -> String {
        match self {
            Self::Commit { message, amended } => {
                let prefix = if *amended { "Amend" } else { "Commit" };
                format!("{prefix}: {}", truncate(message, 40))
            }
            Self::Checkout { to, .. } => format!("Checkout {to}"),
            Self::CreateBranch { name, .. } => format!("Create branch {name}"),
            Self::DeleteBranch { name, .. } => format!("Delete branch {name}"),
            Self::Merge { into, from } => format!("Merge {from} → {into}"),
            Self::Rebase { branch, onto } => format!("Rebase {branch} onto {onto}"),
            Self::Reset { branch, mode, .. } => format!("Reset {branch} ({mode})"),
            Self::StashPush { message } => format!("Stash: {}", truncate(message, 30)),
            Self::StashPop { .. } => "Stash pop".to_string(),
            Self::CherryPick { commits } => format!("Cherry-pick {} commit(s)", commits.len()),
            Self::Revert { commits } => format!("Revert {} commit(s)", commits.len()),
            Self::ForcePush { branch, .. } => format!("Force-push {branch}"),
            Self::Raw { label } => label.clone(),
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoSnapshot {
    /// HEAD oid as hex. Empty string if repo has no commits.
    pub head: String,
    /// HEAD's branch name (without `refs/heads/`). `None` if detached.
    pub head_branch: Option<String>,
    /// Full refname → oid for every non-remote ref we care about.
    pub refs: BTreeMap<String, String>,
    /// True if the working tree had uncommitted changes at capture time.
    pub working_dirty: bool,
}
