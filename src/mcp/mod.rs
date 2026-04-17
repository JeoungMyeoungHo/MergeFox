//! MCP (Model Context Protocol) surface for mergeFox.
//!
//! Exposes a read-only view of the repo's activity log so that AI tools or
//! IDE extensions can inspect the exact sequence of git operations the user
//! has performed recently — great for "something broke, help me figure out
//! what I did" scenarios.
//!
//! Status: **read-only / local-process-only for now.** The JSON schema
//! here is stable; adding the transport (stdio pipe, unix socket, WebSocket)
//! is a follow-up once we're sure of the API shape.
//!
//! Usage (from inside the process, for the UI inspector):
//!
//! ```
//! let view = activity_log::view_for_repo(&journal, ActivityLogQuery::recent(50));
//! ```

pub mod action_preview;
pub mod activity_log;
pub mod forge;
pub mod server;
pub mod types;

pub use action_preview::{preview as preview_action, ActionPreview, ActionRequest, ActionRisk};
pub use activity_log::{view_for_repo, ActivityLogQuery, ActivityLogView};
pub use forge::{
    forge_view_for_state, ForgeIssueView, ForgePullRequestView, ForgeRepoView, ForgeSelectedView,
    ForgeView,
};
pub use types::{ActivityEntry, ActivityOutcome, EntrySummary, RefDelta, TroubleHint};
