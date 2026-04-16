//! Build an activity-log view from a live `Journal`.
//!
//! This is the backend for both (a) the in-app "Activity Log" debug window,
//! and (b) the future MCP transport. It's intentionally pure — no git2
//! access, no filesystem — so we can call it from anywhere a `&Journal`
//! is available.
//!
//! Derived fields (`ref_deltas`, `outcome`, `hints`) are computed here from
//! the raw `before`/`after` RepoSnapshot pairs so external consumers don't
//! have to reinvent them.

use crate::journal::{Journal, JournalEntry, OpSource, Operation, RepoSnapshot};

use super::types::{
    ActivityEntry, ActivityOutcome, EntrySummary, HintSeverity, RefDelta, TroubleHint,
};

#[derive(Debug, Clone)]
pub struct ActivityLogQuery {
    /// How many most-recent entries to return (capped to journal length).
    pub limit: usize,
    /// If set, only entries whose `kind` matches are returned.
    pub only_kind: Option<String>,
    /// If set, only entries whose `source` matches (ui / mcp / external).
    pub only_source: Option<String>,
}

impl ActivityLogQuery {
    pub fn recent(limit: usize) -> Self {
        Self {
            limit,
            only_kind: None,
            only_source: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActivityLogView {
    pub entries: Vec<ActivityEntry>,
    /// Total entries in the underlying journal (not just the returned slice).
    pub total: usize,
    /// 0-based cursor index of the currently-live state (what undo would
    /// step back from). `None` = "void" / before-any-op.
    pub cursor: Option<usize>,
}

/// Build a view for the UI / MCP. Pass an optional repo path for context.
pub fn view_for_repo(journal: &Journal, q: ActivityLogQuery) -> ActivityLogView {
    let total = journal.entries.len();
    let start = total.saturating_sub(q.limit);

    let entries: Vec<ActivityEntry> = journal.entries[start..]
        .iter()
        .filter(|e| {
            q.only_kind
                .as_deref()
                .is_none_or(|k| kind_of(&e.operation) == k)
        })
        .filter(|e| {
            q.only_source
                .as_deref()
                .is_none_or(|s| source_of(&e.source) == s)
        })
        .map(to_activity_entry)
        .collect();

    ActivityLogView {
        entries,
        total,
        cursor: journal.cursor,
    }
}

fn to_activity_entry(e: &JournalEntry) -> ActivityEntry {
    let ref_deltas = diff_refs(&e.before, &e.after);
    let outcome = classify_outcome(&e.before, &e.after, &ref_deltas);
    let hints = derive_hints(&e.operation, &e.before, &e.after, &ref_deltas, &outcome);

    ActivityEntry {
        id: e.id,
        timestamp_unix: e.timestamp_unix,
        label: e.operation.label(),
        kind: kind_of(&e.operation).to_string(),
        source: source_of(&e.source).to_string(),
        summary: EntrySummary {
            head_before: e.before.head.clone(),
            head_after: e.after.head.clone(),
            branch_before: e.before.head_branch.clone(),
            branch_after: e.after.head_branch.clone(),
            working_dirty_before: e.before.working_dirty,
            working_dirty_after: e.after.working_dirty,
            ref_deltas,
        },
        outcome,
        hints,
    }
}

fn kind_of(op: &Operation) -> &'static str {
    match op {
        Operation::Commit { .. } => "commit",
        Operation::Checkout { .. } => "checkout",
        Operation::CreateBranch { .. } => "create_branch",
        Operation::DeleteBranch { .. } => "delete_branch",
        Operation::Merge { .. } => "merge",
        Operation::Rebase { .. } => "rebase",
        Operation::Reset { .. } => "reset",
        Operation::StashPush { .. } => "stash_push",
        Operation::StashPop { .. } => "stash_pop",
        Operation::CherryPick { .. } => "cherry_pick",
        Operation::Revert { .. } => "revert",
        Operation::ForcePush { .. } => "force_push",
        Operation::Raw { .. } => "raw",
    }
}

fn source_of(src: &OpSource) -> &'static str {
    match src {
        OpSource::Ui => "ui",
        OpSource::Mcp { .. } => "mcp",
        OpSource::External => "external",
    }
}

fn diff_refs(before: &RepoSnapshot, after: &RepoSnapshot) -> Vec<RefDelta> {
    let mut out = Vec::new();
    // Refs present in either snapshot with differing values.
    let mut keys: Vec<&String> = before.refs.keys().chain(after.refs.keys()).collect();
    keys.sort();
    keys.dedup();
    for k in keys {
        let b = before.refs.get(k);
        let a = after.refs.get(k);
        if b != a {
            out.push(RefDelta {
                refname: k.clone(),
                before: b.cloned(),
                after: a.cloned(),
            });
        }
    }
    out
}

fn classify_outcome(
    before: &RepoSnapshot,
    after: &RepoSnapshot,
    deltas: &[RefDelta],
) -> ActivityOutcome {
    if deltas.is_empty() && before.head == after.head {
        return ActivityOutcome::NoOp;
    }
    if before.working_dirty && after.working_dirty {
        return ActivityOutcome::PossibleConflict;
    }
    // Fast-forward heuristic: every delta either created a new ref, or
    // the before-value is a prefix of the after-value in git's DAG. We
    // can't do the DAG check without git2, so settle for: all deltas have
    // a non-None `after`. If any delta dropped a ref, call it non-linear.
    let all_forward = deltas.iter().all(|d| d.after.is_some());
    if all_forward {
        ActivityOutcome::FastForward
    } else {
        ActivityOutcome::NonLinear
    }
}

fn derive_hints(
    op: &Operation,
    before: &RepoSnapshot,
    after: &RepoSnapshot,
    deltas: &[RefDelta],
    outcome: &ActivityOutcome,
) -> Vec<TroubleHint> {
    let mut hints = Vec::new();

    // 1. Hard reset — always surface so a curious user sees it.
    if let Operation::Reset { mode, .. } = op {
        if mode == "hard" {
            hints.push(TroubleHint {
                severity: HintSeverity::Warn,
                message: "Hard reset discarded working-tree + index state.".into(),
                suggestion: "If this was unintended, undo (Cmd+Z) or use Panic Recovery to restore the prior snapshot.".into(),
            });
        }
    }

    // 2. Force push — inherently dangerous.
    if let Operation::ForcePush { branch, .. } = op {
        hints.push(TroubleHint {
            severity: HintSeverity::Danger,
            message: format!("Force-pushed {branch} — remote history was rewritten."),
            suggestion:
                "Collaborators on this branch must rebase or reclone. Check the reflog on the remote if recovery is needed."
                    .into(),
        });
    }

    // 3. Checkout that left the working tree dirty.
    if matches!(op, Operation::Checkout { .. }) && after.working_dirty {
        hints.push(TroubleHint {
            severity: HintSeverity::Info,
            message: "Working tree is dirty after checkout.".into(),
            suggestion:
                "mergeFox auto-stashes before destructive navigation; check `git stash list` if files are missing.".into(),
        });
    }

    // 4. `PossibleConflict` outcome — strong signal the op half-applied.
    if matches!(outcome, ActivityOutcome::PossibleConflict) {
        hints.push(TroubleHint {
            severity: HintSeverity::Warn,
            message: "Working tree was dirty both before and after this operation.".into(),
            suggestion:
                "Looks like a merge/cherry-pick conflict. Run `git status` and resolve markers, or undo to try a different approach.".into(),
        });
    }

    // 5. Ref disappeared (e.g. deleted branch while on it).
    let dropped: Vec<&RefDelta> = deltas.iter().filter(|d| d.after.is_none()).collect();
    if !dropped.is_empty() {
        hints.push(TroubleHint {
            severity: HintSeverity::Warn,
            message: format!(
                "{} ref(s) were dropped: {}",
                dropped.len(),
                dropped
                    .iter()
                    .map(|d| d.refname.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            suggestion:
                "Deleted branches still live in the reflog for ~90 days — `git reflog` or mergeFox's Panic Recovery can resurrect the tip."
                    .into(),
        });
    }

    // 6. Detached HEAD after op — informational but surprising.
    if before.head_branch.is_some() && after.head_branch.is_none() {
        hints.push(TroubleHint {
            severity: HintSeverity::Info,
            message: "HEAD is now detached.".into(),
            suggestion: "Create a branch at the current commit if you want to keep the work here."
                .into(),
        });
    }

    hints
}
