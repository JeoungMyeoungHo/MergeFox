//! JSON-serializable types for the MCP activity-log surface.
//!
//! These mirror the internal `journal::JournalEntry` shape but are
//! (a) flatter and (b) carry derived "trouble hints" — heuristics that try
//! to spot suspicious patterns (force-pushes, rapid undo/redo clusters,
//! merge commits on detached HEAD, etc.) so an AI agent can quickly
//! reconstruct the user's intent.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub id: u64,
    pub timestamp_unix: i64,
    /// Human label — same as `Operation::label()`.
    pub label: String,
    /// Machine-readable op kind (snake_case, stable).
    pub kind: String,
    /// Where it came from: ui / mcp / external.
    pub source: String,
    pub summary: EntrySummary,
    pub outcome: ActivityOutcome,
    /// Trouble hints — zero or more heuristic suggestions tied to this entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<TroubleHint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntrySummary {
    pub head_before: String,
    pub head_after: String,
    pub branch_before: Option<String>,
    pub branch_after: Option<String>,
    pub working_dirty_before: bool,
    pub working_dirty_after: bool,
    /// Per-ref deltas (refs that changed target between before/after).
    pub ref_deltas: Vec<RefDelta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefDelta {
    pub refname: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityOutcome {
    /// No observable change — ref set + HEAD identical before/after.
    NoOp,
    /// Moved forward cleanly (all ref changes were fast-forward or new refs).
    FastForward,
    /// Some refs moved non-linearly (e.g. hard reset, force push).
    NonLinear,
    /// Working tree dirty both before + after; suggests the op didn't
    /// fully apply (likely conflicts).
    PossibleConflict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TroubleHint {
    pub severity: HintSeverity,
    pub message: String,
    /// Short "what to try next" nudge aimed at AI agents / users.
    pub suggestion: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HintSeverity {
    Info,
    Warn,
    Danger,
}
