//! `CommitAction` — what the user asked to do from a context menu (or elsewhere).
//!
//! This lives as a flat enum (instead of executing inline) so we can:
//!   1. Route actions through a single dispatcher — necessary for the undo
//!      journal to record every state change in one place.
//!   2. Stub out unimplemented actions with a "TODO" toast during MVP
//!      without breaking the menu wiring.
//!   3. Later have the MCP gateway push the same action type through the
//!      same gate (approval modal → dispatcher).

use gix::ObjectId as Oid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

#[derive(Debug, Clone)]
pub enum CommitAction {
    // --- read-only utility ---
    CopySha(Oid),
    CopyShortSha(Oid),

    // --- navigation / safe writes ---
    Checkout(Oid),
    CheckoutBranch(String),
    CreateBranchPrompt(Oid),
    CreateTagPrompt {
        at: Oid,
        annotated: bool,
    },

    // --- branch ops ---
    /// Cherry-pick one or more commits onto the current branch, in the
    /// order given. Multi-commit cherry-pick runs the single-commit path
    /// in a loop so journaling / conflict surface stays unchanged per
    /// commit — if any pick fails, the loop stops and we surface the
    /// partial state (N of M picked) to the user.
    CherryPick(Vec<Oid>),
    Revert(Oid),
    Reset {
        branch: String,
        mode: ResetMode,
        target: Oid,
    },

    // --- HEAD/amend ops ---
    AmendMessagePrompt,
    /// Reword the message of any reachable commit (not just HEAD).
    /// Opens a modal; on confirm the worker thread runs
    /// `git::reword_commit`, which auto-stashes, creates a backup tag,
    /// rewrites the target commit's message, and rebases the
    /// descendants on top. Valid for any commit reachable from the
    /// current branch.
    RewordPrompt(Oid),
    DropCommitPrompt(Oid),
    MoveCommitUp(Oid),
    MoveCommitDown(Oid),

    // --- split commit ---
    /// Open the split-commit wizard against the given commit. The
    /// dispatcher discovers hunks synchronously (cheap — one `git
    /// show` call) and opens the modal with the resulting plan skeleton.
    SplitCommit(Oid),

    // --- branch-tip ops ---
    Pull {
        branch: String,
    },
    Push {
        branch: String,
        force: bool,
    },
    /// Push a single tag to the default remote. The dispatcher
    /// resolves which remote by looking up the repo's configured
    /// default (falling back to `origin`).
    PushTag {
        tag: String,
    },
    /// `git push <remote> --tags` — every local tag not yet on the
    /// remote. Dispatcher shows a pre-flight that counts outgoing
    /// tags so the user knows what they're about to broadcast.
    PushAllTags,
    SetUpstreamPrompt {
        branch: String,
    },
    RenameBranchPrompt {
        from: String,
    },
    DeleteBranchPrompt {
        name: String,
        is_remote: bool,
    },

    // --- creation from commit ---
    CreateWorktreePrompt(Oid),

    // --- stash ---
    /// Prompt for a stash message and create a new stash including
    /// untracked files. Triggered from the stash "+" button in the sidebar.
    StashPushPrompt,
    /// Apply-and-drop the stash at this 0-based index.
    StashPop {
        index: usize,
    },
    /// Apply (without dropping) the stash at this 0-based index.
    StashApply {
        index: usize,
    },
    /// Delete the stash at this 0-based index. Confirmation required.
    StashDropPrompt {
        index: usize,
        message: String,
    },
}

impl CommitAction {
    /// Short label used for toast messages when the action is stubbed.
    pub fn describe(&self) -> String {
        match self {
            Self::CopySha(o) => format!("copy SHA {o}"),
            Self::CopyShortSha(o) => format!("copy short SHA {}", short(o)),
            Self::Checkout(o) => format!("checkout {}", short(o)),
            Self::CheckoutBranch(b) => format!("checkout branch {b}"),
            Self::CreateBranchPrompt(o) => format!("create branch at {}", short(o)),
            Self::CreateTagPrompt { at, annotated } => {
                format!(
                    "{} tag at {}",
                    if *annotated {
                        "create annotated"
                    } else {
                        "create"
                    },
                    short(at)
                )
            }
            Self::CherryPick(ids) => {
                if ids.len() == 1 {
                    format!("cherry-pick {}", short(&ids[0]))
                } else {
                    format!("cherry-pick {} commits", ids.len())
                }
            }
            Self::Revert(o) => format!("revert {}", short(o)),
            Self::Reset {
                branch,
                mode,
                target,
            } => {
                format!("reset {branch} [{mode:?}] → {}", short(target))
            }
            Self::AmendMessagePrompt => "amend commit message".to_string(),
            Self::RewordPrompt(o) => format!("reword commit {}", short(o)),
            Self::DropCommitPrompt(o) => format!("drop commit {}", short(o)),
            Self::MoveCommitUp(o) => format!("move {} up", short(o)),
            Self::MoveCommitDown(o) => format!("move {} down", short(o)),
            Self::SplitCommit(o) => format!("split commit {}", short(o)),
            Self::Pull { branch } => format!("pull {branch}"),
            Self::Push { branch, force } => {
                format!("{} {branch}", if *force { "force-push" } else { "push" })
            }
            Self::PushTag { tag } => format!("push tag {tag}"),
            Self::PushAllTags => "push all tags".to_string(),
            Self::SetUpstreamPrompt { branch } => format!("set upstream for {branch}"),
            Self::RenameBranchPrompt { from } => format!("rename branch {from}"),
            Self::DeleteBranchPrompt { name, is_remote } => {
                format!(
                    "delete {} branch {name}",
                    if *is_remote { "remote" } else { "local" }
                )
            }
            Self::CreateWorktreePrompt(o) => format!("create worktree from {}", short(o)),
            Self::StashPushPrompt => "create stash".to_string(),
            Self::StashPop { index } => format!("pop stash@{{{index}}}"),
            Self::StashApply { index } => format!("apply stash@{{{index}}}"),
            Self::StashDropPrompt { index, .. } => format!("drop stash@{{{index}}}"),
        }
    }
}

fn short(o: &Oid) -> String {
    let s = o.to_string();
    s[..7.min(s.len())].to_string()
}
