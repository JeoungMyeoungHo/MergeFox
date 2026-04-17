use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};

use crate::clone::CloneHandle;
use crate::config::{Config, PullStrategyPref, RepoSettings, ThemeSettings, UiLanguage};
use crate::forge::{ForgeCreateIssueResult, ForgeCreatePrResult, ForgeRefreshResult, ForgeState};
use crate::git::{
    BranchInfo, GitCapability, GitJob, GitJobKind, GraphScope, Repo, RepoDiff, StashEntry,
    StatusEntry,
};
use crate::journal::{self, Journal, JournalEntry, OpSource, Operation, RepoSnapshot};
use crate::providers;
use crate::ui;
use crate::ui::columns::ColumnPrefs;
use crate::ui::graph::GraphView;

pub struct MergeFoxApp {
    pub view: View,
    pub config: Config,
    /// Cloned at app-init time from `CreationContext::egui_ctx`. Lives
    /// alongside `self` so any background-thread spawn can call
    /// `ctx.request_repaint()` the moment its work completes — without
    /// this, egui's idle scheduler can leave a 1–3 s gap between the
    /// worker finishing and the UI noticing, and every click feels
    /// laggy even though `diff_for_commit` itself runs in ~80 ms.
    pub egui_ctx: egui::Context,
    /// Process-wide credential store with in-memory cache. All AI API
    /// keys, provider PATs and OAuth tokens go through here so the OS
    /// keychain consent prompt fires at most once per session per entry
    /// (vs. every Settings open / commit-modal / API call under the
    /// naive `keyring::Entry` path).
    pub secret_store: Arc<crate::secrets::SecretStore>,
    pub git_capability: GitCapability,
    pub last_error: Option<String>,
    pub hud: Option<Hud>,
    /// Unified notification queue. New call sites should use
    /// `self.notify_ok / notify_info / notify_warn / notify_err`
    /// instead of touching `hud` or `last_error` directly — those two
    /// still exist for the journal HUD + legacy paths but are being
    /// migrated out. See `ui::notifications` for the queue.
    pub notifications: crate::ui::notifications::NotificationCenter,
    /// True once the first `update` frame has fired startup-only
    /// notifications (git-missing banner today; future additions
    /// follow the same gate). Flips at most once per process.
    pub startup_notified: bool,
    /// Blame modal state — current task, last result, error. Lives
    /// on the app so the modal survives frame-to-frame re-renders.
    pub blame: crate::ui::blame::BlameState,
    /// Bisect modal state (buttons + lazy-loaded `BisectStatus`).
    pub bisect_ui: crate::ui::bisect::BisectUiState,
    /// Installed lazily because most repos never open an image diff.
    pub image_loaders_installed: bool,
    /// Times of recent undo/redo nav steps — used to detect panic spam.
    pub nav_history: VecDeque<Instant>,
    /// True if a panic-recovery modal should be open.
    pub panic_modal_open: bool,
    /// True when the commit dialog should render.
    pub commit_modal_open: bool,
    /// Persistent state for the commit modal (typed message, errors).
    pub commit_modal: Option<CommitModal>,
    /// Text-input / confirmation modal driven by context-menu actions.
    /// At most one at a time; `None` = no modal.
    pub pending_prompt: Option<crate::ui::prompt::PendingPrompt>,
    /// True when the column-picker popover is open (see ui::columns).
    pub columns_popover_open: bool,
    /// True when the MCP activity-log inspector modal is open.
    pub activity_log_open: bool,
    /// True when the reflog recovery window should render.
    pub reflog_open: bool,
    /// True when the keyboard-shortcuts cheatsheet should render.
    /// Opened with `?` and closed with Esc or the window `x`.
    pub shortcuts_open: bool,
    /// True when the command palette (⌘K) is visible. Query + selected
    /// index live alongside so they survive re-renders without being
    /// recomputed from scratch each frame.
    pub palette_open: bool,
    pub palette_query: String,
    pub palette_selected: usize,
    /// True when the settings window should render.
    pub settings_open: bool,
    /// Persistent in-window edits for the settings modal.
    pub settings_modal: Option<SettingsModal>,
    /// Publish flow for "create hosted repo from this local checkout".
    pub publish_remote_modal: Option<PublishRemoteModal>,
    /// Running "Test endpoint" probe from the AI settings section. Polled
    /// every frame while Settings is open; the handle is consumed once.
    pub ai_test_task:
        Option<crate::ai::AiTask<crate::ai::error::Result<crate::ai::CompletionResponse>>>,
    /// Running OAuth device-code bootstrap from Settings → Integrations.
    pub provider_oauth_start_task: Option<
        crate::providers::runtime::ProviderTask<
            crate::providers::ProviderResult<crate::ui::settings::integrations::OAuthStartOutcome>,
        >,
    >,
    /// Running OAuth device-code token polling / profile fetch.
    pub provider_oauth_poll_task: Option<
        crate::providers::runtime::ProviderTask<
            crate::providers::ProviderResult<
                crate::ui::settings::integrations::OAuthConnectOutcome,
            >,
        >,
    >,
    /// Running "Generate commit message" job owned by the commit modal.
    /// Kept on `MergeFoxApp` (not on the modal) so the modal can stay
    /// `Default`-constructible and the task survives trivial remounts.
    pub commit_ai_task: Option<
        crate::ai::AiTask<
            crate::ai::error::Result<crate::ai::tasks::commit_message::CommitSuggestion>,
        >,
    >,
    /// Loading forge metadata + PR/issue lists for the active repository.
    pub forge_refresh_task:
        Option<providers::runtime::ProviderTask<providers::ProviderResult<ForgeRefreshResult>>>,
    pub forge_create_pr_task:
        Option<providers::runtime::ProviderTask<providers::ProviderResult<ForgeCreatePrResult>>>,
    pub forge_create_issue_task:
        Option<providers::runtime::ProviderTask<providers::ProviderResult<ForgeCreateIssueResult>>>,
}

#[derive(Default)]
pub struct CommitModal {
    pub message: String,
    pub last_error: Option<String>,
    /// Last error from the AI ✨ Generate button, shown under the row.
    /// Separate from `last_error` because a failed commit and a failed
    /// AI call mean different things to the user.
    pub ai_error: Option<String>,
    /// User-selected paths (for bulk stage / unstage). Keyed by the
    /// path's display string so we don't have to re-key whenever the
    /// entries list rebuilds. Selection survives across `status` polls.
    pub selection: std::collections::BTreeSet<std::path::PathBuf>,
    /// Last clicked path used as the range-selection anchor for
    /// Shift-click in the commit dialog.
    pub selection_anchor: Option<std::path::PathBuf>,
    /// Repo this amend-author snapshot was loaded from.
    pub amend_author_repo_path: Option<std::path::PathBuf>,
    /// Whether the active repo has a HEAD commit we can amend.
    pub amend_head_available: bool,
    /// Current HEAD author shown as the amend baseline.
    pub amend_head_author_name: String,
    pub amend_head_author_email: String,
    /// Optional author override applied only to `git commit --amend`.
    pub amend_author_override: bool,
    pub amend_author_name: String,
    pub amend_author_email: String,
}

pub struct SettingsModal {
    /// Which left-sidebar category is active. Persists while the window is
    /// open; reset to `General` when (re)opened.
    pub section: ui::settings::SettingsSection,
    pub search_query: String,
    pub language: UiLanguage,
    pub theme: ThemeSettings,
    pub repo_path: Option<PathBuf>,
    pub default_remote: Option<String>,
    pub pull_strategy: PullStrategyPref,
    pub remotes: Vec<RemoteDraft>,
    pub new_remote_name: String,
    pub new_fetch_url: String,
    pub new_push_url: String,
    /// In-progress edits for provider/account connections.
    pub integrations: ui::settings::integrations::IntegrationsDraft,
    /// In-progress edits for the AI section. We keep the draft separate
    /// from `config.ai_endpoint` so the user can tweak fields without
    /// committing them — Save applies the draft back to config + keyring.
    pub ai: ui::settings::ai::AiDraft,
    /// Unified feedback banner — `Ok("saved")` or `Err("...")`, never both.
    /// Older `last_error` / `notice` split led to states where both could be
    /// set at once after a refresh failure.
    pub feedback: Option<ui::settings::Feedback>,
    /// Per-repo provider account slug. `None` = auto-detect.
    pub provider_account_slug: Option<String>,
    // --- Git identity ---
    /// Lazy-loaded from `git config user.name` on first render.
    pub identity_name: String,
    pub identity_email: String,
    /// Write to `--global` instead of the repo-local config.
    pub identity_global: bool,
    /// True once the initial read from git config has happened.
    pub identity_loaded: bool,
    /// Lazy-loaded worktree list — `None` until first render triggers
    /// a refresh, then `Some(list)` (possibly empty if only the main
    /// worktree exists).
    pub worktrees: Option<Vec<crate::git::WorktreeInfo>>,
    /// Lazy-loaded `git count-objects -v` summary powering the
    /// maintenance section's "N KiB loose · M KiB packed" banner.
    /// Cleared to `None` after gc/repack so the banner re-reads.
    pub count_objects: Option<crate::git::CountObjectsSummary>,
    /// Lazy-loaded sparse-checkout state. Patterns are edited as a
    /// multi-line string (`sparse_patterns_draft`) and applied
    /// through the handler — we keep the last-read status around so
    /// we can detect which patterns were added / removed for the
    /// `cone` vs classic-mode warning copy.
    pub sparse_checkout: Option<crate::git::SparseCheckoutStatus>,
    pub sparse_patterns_draft: String,
    /// Lazy-loaded submodule list. `None` until first render, then
    /// `Some(list)` (empty when the repo has no submodules). Reset
    /// after update / sync to force a re-read.
    pub submodules: Option<Vec<crate::git::SubmoduleEntry>>,
    /// Most recent on-screen size of the settings window. Saved back to
    /// config when the modal closes so the next open restores it.
    pub window_size: Option<crate::config::SettingsWindowState>,
    pub reset_scope: Option<ui::settings::SettingsResetScope>,
}

pub struct PublishRemoteModal {
    pub repo_path: PathBuf,
    pub branch: String,
    pub selected_account: Option<providers::AccountId>,
    pub owners: Vec<providers::RemoteRepoOwner>,
    pub owners_task: Option<
        providers::runtime::ProviderTask<
            providers::ProviderResult<Vec<providers::RemoteRepoOwner>>,
        >,
    >,
    pub create_task: Option<
        providers::runtime::ProviderTask<
            providers::ProviderResult<providers::CreatedRepositoryRef>,
        >,
    >,
    pub selected_owner: Option<String>,
    pub remote_name: String,
    pub repository_name: String,
    pub description: String,
    pub private: bool,
    pub last_error: Option<String>,
}

#[derive(Clone)]
pub struct RemoteDraft {
    pub name: String,
    pub fetch_url: String,
    pub push_url: String,
    /// Live input buffer for the "Rename remote" inline row. We keep the
    /// old name in `name` (that's what `git remote rename` needs as the
    /// source), and the proposed new name here. Empty or equal to `name`
    /// means "no rename requested".
    pub rename_to: String,
}

pub enum View {
    Welcome(WelcomeState),
    /// Repository-open in progress on a background thread. We hold the
    /// channel + a label so the UI can show "Opening repository… /
    /// Building graph…" status instead of silently freezing while
    /// gix loads the packed-refs + we walk commits. Lives here
    /// (not inside Workspace) because, for the first open, there's no
    /// workspace yet.
    OpeningRepo(OpeningRepoState),
    /// One-or-more open repos, with an index into the active one. We keep
    /// the enum flat (no per-tab Welcome) — if the user closes the last
    /// tab, we fall back to `View::Welcome`.
    Workspace(WorkspaceTabs),
}

pub struct OpeningRepoState {
    pub path: PathBuf,
    pub started_at: Instant,
    /// Latest stage message, written by the worker thread. Rendered in
    /// the loading UI each frame; `Arc<Mutex<String>>` so the worker
    /// can update it without returning control.
    pub label: Arc<std::sync::Mutex<String>>,
    /// Receiver for the final outcome. `CommitGraph` is `Send`; we
    /// re-open the `Repo` on the main thread after receiving (cheap
    /// now because gix's packed-refs is already in the OS page
    /// cache from the worker's first open).
    pub rx: std::sync::mpsc::Receiver<OpenOutcome>,
    /// Workspace state stashed away while we transitionally render the
    /// loading view. When the open finishes we append the new tab to
    /// this existing WorkspaceTabs instead of throwing it away; when it
    /// fails we restore it so the user doesn't lose their other tabs.
    /// `None` = user opened from Welcome, not from an existing
    /// workspace.
    pub preserved_tabs: Option<WorkspaceTabs>,
}

pub enum OpenOutcome {
    Ok {
        path: PathBuf,
        graph: crate::git::CommitGraph,
    },
    Err(String),
}

pub struct WorkspaceTabs {
    pub tabs: Vec<WorkspaceState>,
    pub active: usize,
    /// Optional launcher tab used for opening / cloning another repository
    /// without leaving the current workspace tabs.
    pub launcher_tab: Option<WelcomeState>,
    pub launcher_active: bool,
}

impl WorkspaceTabs {
    pub fn current(&self) -> &WorkspaceState {
        &self.tabs[self.active]
    }
    pub fn current_mut(&mut self) -> &mut WorkspaceState {
        &mut self.tabs[self.active]
    }
}

#[derive(Default)]
pub struct WelcomeState {
    pub input: String,
    pub clone: Option<CloneHandle>,
    pub remote_repos: RemoteRepoBrowserState,
    /// Background size probe running against a hosted provider before we
    /// kick off the clone. Present only while waiting for the API reply;
    /// cleared once the result is drained into `clone_size_prompt` or
    /// we proceed straight to `clone`.
    pub clone_preflight: Option<crate::clone::ClonePreflightHandle>,
    /// User-facing prompt state when a large-repo preflight came back
    /// above the configured threshold. Rendering this modal pauses the
    /// welcome flow until the user picks Shallow / Full / Cancel.
    pub clone_size_prompt: Option<CloneSizePrompt>,
}

/// "We looked up the repo size and it's big enough to ask you."
#[derive(Debug, Clone)]
pub struct CloneSizePrompt {
    pub url: String,
    pub dest: PathBuf,
    pub size_bytes: u64,
    /// Depth that `Shallow` will use — propagated from the active clone
    /// defaults at the time the prompt was constructed so the value can't
    /// change under the user while they're deciding.
    pub shallow_depth: u32,
}

pub struct CreateRemoteRepoState {
    pub open: bool,
    pub owners: Vec<providers::RemoteRepoOwner>,
    pub owners_task: Option<
        providers::runtime::ProviderTask<
            providers::ProviderResult<Vec<providers::RemoteRepoOwner>>,
        >,
    >,
    pub create_task: Option<
        providers::runtime::ProviderTask<
            providers::ProviderResult<providers::CreatedRepositoryRef>,
        >,
    >,
    pub selected_owner: Option<String>,
    pub name: String,
    pub description: String,
    pub private: bool,
    pub auto_init: bool,
    pub last_error: Option<String>,
    pub last_created: Option<providers::CreatedRepositoryRef>,
}

impl Default for CreateRemoteRepoState {
    fn default() -> Self {
        Self {
            open: false,
            owners: Vec::new(),
            owners_task: None,
            create_task: None,
            selected_owner: None,
            name: String::new(),
            description: String::new(),
            private: true,
            auto_init: false,
            last_error: None,
            last_created: None,
        }
    }
}

#[derive(Default)]
pub struct RemoteRepoBrowserState {
    pub selected_account: Option<providers::AccountId>,
    pub repos: Vec<providers::RemoteRepoSummary>,
    pub task: Option<
        providers::runtime::ProviderTask<
            providers::ProviderResult<Vec<providers::RemoteRepoSummary>>,
        >,
    >,
    pub last_error: Option<String>,
    pub loaded_once: bool,
    pub create_repo: CreateRemoteRepoState,
}

pub struct WorkspaceState {
    pub repo: Repo,
    pub selected_branch: Option<String>,
    pub graph_scope: GraphScope,
    pub graph_view: Option<GraphView>,
    pub journal: Option<Journal>,
    /// Background fetch / push / pull. At most one at a time for now.
    pub active_job: Option<GitJob>,
    /// Currently-selected commit in the graph (drives the diff panel).
    pub selected_commit: Option<gix::ObjectId>,
    /// Diff of `selected_commit` vs its first parent, if available.
    pub current_diff: Option<Arc<RepoDiff>>,
    /// Which file in `current_diff.files` the user is viewing in the
    /// center pane. `None` means the commit is selected but the user
    /// has not opened any file yet, so the graph stays visible.
    pub selected_file_idx: Option<usize>,
    /// Whether the center pane shows the patch or the raw file snapshot.
    pub selected_file_view: SelectedFileView,
    /// `true` = show commit files grouped by directory (tree view).
    /// `false` = flat list sorted by path (default).
    pub file_list_tree: bool,
    /// Lazily-loaded bytes for the currently selected image diff only.
    ///
    /// **Do not assign to this field directly** — use
    /// `WorkspaceState::set_image_cache` so the previous cache's URIs get
    /// queued for eviction from egui's loader cache. Direct assignment
    /// silently leaks GPU textures across commit selections (egui keys
    /// its image cache by URI, not by Arc ownership, so dropping the
    /// `Arc<[u8]>` here doesn't free the decoded texture).
    pub selected_image_cache: Option<SelectedImageCache>,
    /// Cached blob text + per-line byte offsets for the snapshot view.
    /// Keyed by blob oid so selecting a different file / commit
    /// invalidates it automatically. Without this cache we re-read the
    /// blob from the git object DB and re-split it into lines every
    /// frame — fine for 100-line files, painful on the 30k-line files
    /// that show up in monorepos.
    pub snapshot_cache: Option<SnapshotCache>,
    /// URIs from previously-held image caches, waiting for the next frame
    /// to call `ctx.forget_image` on them. Processed at the top of
    /// `MergeFoxApp::update`.
    pub pending_image_evictions: Vec<String>,
    /// Selected conflict file in the conflict-resolution window.
    pub selected_conflict: Option<PathBuf>,
    /// In-progress manual resolution buffer for the selected conflict.
    pub conflict_editor_path: Option<PathBuf>,
    /// Editor contents for `conflict_editor_path`.
    pub conflict_editor_text: String,
    /// Modal state for planning an interactive rebase.
    pub rebase_modal: Option<RebaseModalState>,
    /// Active linear rebase replay session, if one is in progress.
    pub rebase_session: Option<RebaseSession>,
    /// Column visibility / compact-mode preferences for the graph.
    pub column_prefs: ColumnPrefs,
    /// Short label used on the tab strip — usually the repo folder name.
    pub tab_title: String,
    /// Forge/hosting integration state for this repository tab.
    pub forge: ForgeState,
    /// Background "should this be Git LFS?" scan. `running` holds the
    /// receiver while a scan is in flight; `result` is the latest finished
    /// scan (kept across frames so the sidebar can render it). Both are
    /// cleared when the workspace is closed or the user dismisses the hint.
    pub lfs_scan: LfsScanState,
    /// In-flight async undo / redo / restore. Cmd+Z / Cmd+Shift+Z spawn a
    /// `JournalNavTask` instead of running the work on the UI thread, so
    /// big-binary repos don't freeze the window for seconds. While this
    /// is `Some`, subsequent nav requests are coalesced (no queue) and
    /// the user gets a "still navigating…" hint.
    pub nav_task: Option<crate::journal::JournalNavTask>,
    /// In-flight background diff computation. Set when the user clicks a
    /// commit; cleared when the result lands in `current_diff`. Linux-
    /// kernel merge commits can produce 5000+ file diffs where git's
    /// rename detection runs for several seconds, so computing on the UI
    /// thread would freeze every click. Instead we spawn a worker and
    /// show "Computing diff…" until it returns.
    pub diff_task: Option<DiffTask>,
    /// Most-recent click the user made that we haven't yet spawned a
    /// worker for — because one is already in flight for some OTHER
    /// commit. When the current worker finishes we pick up this oid
    /// and spawn for it, discarding any intermediate clicks that got
    /// overwritten. This is how rapid "click through 10 commits"
    /// browsing stays to 1 git subprocess instead of 10 racing.
    pub pending_diff_oid: Option<gix::ObjectId>,
    /// Bounded LRU of recently-computed diffs, keyed by commit oid.
    /// Clicking back onto a commit the user already visited is then a
    /// HashMap lookup, not a subprocess spawn — big win for the common
    /// "flip between two commits to compare" pattern.
    pub diff_cache: DiffCache,
    /// In-flight background graph (re)build. Set by `rebuild_graph` and
    /// `restore_active_graph_cache` so that a graph refresh triggered by
    /// an op (merge, rebase, tab restore) doesn't freeze the UI thread
    /// while `CommitGraph::build` walks up to `MAX_GRAPH_COMMITS`. The
    /// previous `graph_view` stays visible until the worker completes.
    pub graph_task: Option<GraphTask>,
    /// Snapshot of the sidebar / top-bar / working-tree info shared
    /// across every paint. These used to be recomputed each frame
    /// inside `sidebar::show` and `main_panel::show` via expensive
    /// gix / git calls (`repo.statuses` walks the entire working tree
    /// including untracked, `list_branches` scans every ref, etc.).
    /// At 60 fps on a moderately large repo this was 50–150 ms of
    /// gix / git work per frame, which made commit clicks feel sticky
    /// because the click frame couldn't finish before the worker's
    /// "Computing diff…" panel appeared. We now refresh this cache
    /// only on repo mutation (ops, rebuild_graph, tab open).
    pub repo_ui_cache: Option<RepoUiCache>,
    /// Last time we polled `git status` for out-of-band working tree
    /// changes (edits from another editor / generator / terminal).
    pub last_working_tree_poll: Instant,
    /// Last time we checked HEAD/refs/index for out-of-band repo changes
    /// from another git client.
    pub last_repo_external_poll: Instant,
    /// Snapshot of HEAD/ref/index markers used to detect external repo
    /// changes without spawning git every frame.
    pub repo_external_snapshot: Option<RepoExternalSnapshot>,
    /// Whether the working tree changes section is expanded in the main panel.
    /// When expanded, file list is shown inline above the graph.
    pub working_tree_expanded: bool,
    /// Selected working tree file for inline diff preview.
    /// Stored as path string to persist across frames.
    pub selected_working_file: Option<std::path::PathBuf>,
    /// Diff of the selected working tree file (staged or unstaged).
    pub working_file_diff: Option<String>,
    /// Whether the Working Tree virtual node is selected (like a commit selection).
    /// When true, the diff panel shows working tree changes instead of a commit.
    pub selected_working_tree: bool,
}

/// Per-tab cached view of repo state. Populated by
/// `MergeFoxApp::refresh_repo_ui_cache` after mutations.
pub struct RepoUiCache {
    pub branches: Vec<BranchInfo>,
    pub branch_error: Option<String>,
    pub stashes: Option<Vec<StashEntry>>,
    pub stash_error: Option<String>,
    pub working: Option<Vec<StatusEntry>>,
    pub working_error: Option<String>,
    pub remotes: Vec<String>,
    /// How many commits HEAD is ahead of its upstream tracking ref.
    /// 0 when there's no upstream.
    pub ahead: usize,
    /// How many commits HEAD is behind its upstream.
    pub behind: usize,
    /// Non-fatal tracking computation failure (for example git missing).
    pub tracking_error: Option<String>,
}

const WORKING_TREE_POLL_INTERVAL: Duration = Duration::from_millis(700);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepoExternalSnapshot {
    pub head_name: Option<String>,
    pub head_oid: Option<gix::ObjectId>,
    pub head_file_mtime_ns: Option<u128>,
    pub packed_refs_mtime_ns: Option<u128>,
    pub refs_heads_mtime_ns: Option<u128>,
    pub refs_remotes_mtime_ns: Option<u128>,
    pub fetch_head_mtime_ns: Option<u128>,
    pub stash_log_mtime_ns: Option<u128>,
    pub index_mtime_ns: Option<u128>,
}

pub struct GraphTask {
    /// Scope this task is computing. Compared against `ws.graph_scope`
    /// when the result lands, so a stale rebuild (user changed scope
    /// again while this one was running) is discarded instead of
    /// installed.
    pub scope: GraphScope,
    pub rx: std::sync::mpsc::Receiver<std::result::Result<crate::git::CommitGraph, String>>,
}

/// File-snapshot text cache used by the "File View" toggle in the diff
/// panel. Holds the decoded blob once per file selection so the
/// per-frame renderer can look up line `i` in O(1) without re-splitting
/// the whole file or re-reading the blob via gix.
pub struct SnapshotCache {
    /// Blob oid this cache was built from. When the user selects a
    /// different file or commit the caller compares the new oid against
    /// this and drops the cache on mismatch.
    pub oid: Option<gix::ObjectId>,
    pub text: Arc<str>,
    /// Byte ranges into `text`, one entry per line (excluding the
    /// trailing newline). Parallel to the line numbers shown in the
    /// gutter.
    pub line_bounds: Vec<(u32, u32)>,
}

pub struct DiffTask {
    /// The commit whose diff we're computing. Used so late-arriving
    /// results for an already-abandoned selection (user clicked
    /// another commit before this one finished) can be dropped instead
    /// of displayed.
    pub oid: gix::ObjectId,
    pub started_at: Instant,
    pub rx: std::sync::mpsc::Receiver<std::result::Result<crate::git::RepoDiff, String>>,
}

/// Bounded FIFO cache of recently-computed commit diffs.
///
/// The eviction order is insertion-order, not access-order — this is
/// deliberate: maintaining LRU bookkeeping on every lookup would
/// require interior mutability on every paint. Insertion-order FIFO is
/// a pretty good approximation for the "browse recent commits" use case
/// and keeps the type `Clone + Default` without extra wrappers.
pub struct DiffCache {
    entries: std::collections::HashMap<gix::ObjectId, Arc<RepoDiff>>,
    order: std::collections::VecDeque<gix::ObjectId>,
    capacity: usize,
}

impl DiffCache {
    pub fn get(&self, oid: &gix::ObjectId) -> Option<Arc<RepoDiff>> {
        self.entries.get(oid).cloned()
    }

    pub fn insert(&mut self, oid: gix::ObjectId, diff: Arc<RepoDiff>) {
        if self.entries.contains_key(&oid) {
            // Bump to the newest slot in the eviction queue so "you just
            // viewed this" beats "you viewed this 30 commits ago" for
            // eviction purposes.
            self.order.retain(|o| *o != oid);
        }
        self.entries.insert(oid, diff);
        self.order.push_back(oid);
        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.entries.remove(&old);
            }
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}

impl Default for DiffCache {
    fn default() -> Self {
        // 32 recent diffs ~ a few MB RAM; covers typical browsing patterns
        // without letting a long session accumulate unboundedly.
        Self {
            entries: std::collections::HashMap::with_capacity(32),
            order: std::collections::VecDeque::with_capacity(32),
            capacity: 32,
        }
    }
}

impl WorkspaceState {
    /// Replace `selected_image_cache`, queueing the outgoing cache's URIs
    /// for eviction from egui's loader cache.
    ///
    /// egui's `bytes://...` loader keeps the decoded GPU texture under
    /// the URI key, so simply dropping the old `SelectedImageCache`
    /// (which owns only the raw `Arc<[u8]>`) leaves the texture alive
    /// until the app exits. Over a long review session that accumulates
    /// into tens of MB of unreachable GPU memory. By enqueuing the URIs
    /// here and calling `ctx.forget_image` at the top of the next frame,
    /// we release the texture promptly.
    ///
    /// If `new_cache` is identical to the existing one (same oids and
    /// extension), we skip the eviction — egui will reuse the live
    /// texture and avoid a round-trip through its image loader.
    pub fn set_image_cache(&mut self, new_cache: Option<SelectedImageCache>) {
        if caches_match(&self.selected_image_cache, &new_cache) {
            self.selected_image_cache = new_cache;
            return;
        }
        if let Some(old) = self.selected_image_cache.take() {
            for uri in old.uris() {
                self.pending_image_evictions.push(uri);
            }
        }
        self.selected_image_cache = new_cache;
    }
}

fn caches_match(a: &Option<SelectedImageCache>, b: &Option<SelectedImageCache>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.old_oid == b.old_oid && a.new_oid == b.new_oid && a.ext == b.ext,
        (None, None) => true,
        _ => false,
    }
}

/// What kind of navigation `spawn_nav` should perform. Internal to
/// `MergeFoxApp` — callers use the convenience methods (`undo`, `redo`,
/// `restore_to_entry`).
enum NavRequest {
    Undo,
    Redo,
    RestoreToBefore { entry_id: u64 },
}

/// Two-phase background LFS scan: spawn a thread on repo open, poll the
/// receiver each frame, store the result for the sidebar to read.
#[derive(Default)]
pub struct LfsScanState {
    pub running: Option<std::sync::mpsc::Receiver<crate::git::LfsScanResult>>,
    pub result: Option<crate::git::LfsScanResult>,
    /// User dismissed the hint for this session. We don't persist this —
    /// next app start surfaces the hint again so it isn't permanently
    /// silenced if the underlying problem remains.
    pub dismissed: bool,
}

#[derive(Clone)]
pub struct SelectedImageCache {
    pub old_oid: Option<gix::ObjectId>,
    pub new_oid: Option<gix::ObjectId>,
    pub old_bytes: Option<Arc<[u8]>>,
    pub new_bytes: Option<Arc<[u8]>>,
    /// Lowercase extension (e.g. "png", "jpg"). Retained on the cache so
    /// we can reconstruct the `bytes://diff/<oid>.<ext>` URIs that
    /// `paint_image_pane` registers with egui's image loader — without
    /// this, we couldn't evict the cached GPU texture when the user
    /// switches to another image.
    pub ext: String,
}

impl SelectedImageCache {
    /// The URIs this cache registered with egui's loader cache. Used by
    /// `WorkspaceState::set_image_cache` to evict the decoded GPU
    /// textures when the cache is replaced — otherwise each new diff
    /// accumulates a texture in egui's loader indefinitely.
    ///
    /// We skip entries without an oid (anonymous, rare) because their URI
    /// is derived from a now-invalid pointer value; missing an eviction
    /// for them just means the texture sits in egui's cache until the
    /// app closes, which is acceptable because they don't accumulate in
    /// normal commit-browsing flow.
    pub fn uris(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(2);
        if let Some(oid) = self.old_oid {
            out.push(format!("bytes://diff/{oid}.{}", self.ext));
        }
        if let Some(oid) = self.new_oid {
            out.push(format!("bytes://diff/{oid}.{}", self.ext));
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectedFileView {
    Diff,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseAction {
    Pick,
    Reword,
    Squash,
    /// Fixup — like Squash, but discards this commit's message and
    /// uses only the previous (kept) commit's message. Matches the
    /// `fixup` instruction in `git rebase -i`. Dropdown shows this
    /// separately from Squash so the user doesn't have to remember
    /// which one eats the message.
    Fixup,
    Drop,
}

impl RebaseAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pick => "Pick",
            Self::Reword => "Reword",
            Self::Squash => "Squash",
            Self::Fixup => "Fixup",
            Self::Drop => "Drop",
        }
    }
    /// Does this action roll into its parent's commit in the final
    /// history? Both Squash and Fixup return true — the UI uses this
    /// for the bracket line showing the destination.
    pub fn rolls_into_parent(self) -> bool {
        matches!(self, Self::Squash | Self::Fixup)
    }
}

#[derive(Debug, Clone)]
pub struct RebasePlanItem {
    pub oid: gix::ObjectId,
    pub summary: String,
    pub author: String,
    pub timestamp: i64,
    pub action: RebaseAction,
    pub original_message: String,
    pub edited_message: String,
}

pub struct RebaseModalState {
    pub branch: String,
    pub base: gix::ObjectId,
    pub backup_current_state: bool,
    pub items: Vec<RebasePlanItem>,
    pub selected_idx: usize,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RebaseSessionStep {
    pub oid: gix::ObjectId,
    pub action: RebaseAction,
    pub message: String,
}

pub struct RebaseSession {
    pub branch: String,
    pub base: gix::ObjectId,
    pub backup_ref: Option<String>,
    pub steps: Vec<RebaseSessionStep>,
    pub next_index: usize,
    pub before_snapshot: Option<RepoSnapshot>,
}

pub struct Hud {
    pub message: String,
    pub action: Option<HudAction>,
    pub shown_at: Instant,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub struct HudAction {
    pub label: String,
    pub intent: HudIntent,
}

#[derive(Debug, Clone, Copy)]
pub enum HudIntent {
    OpenSettings(ui::settings::SettingsSection),
}

impl Hud {
    pub fn new(msg: impl Into<String>, duration_ms: u64) -> Self {
        Self {
            message: msg.into(),
            action: None,
            shown_at: Instant::now(),
            duration_ms,
        }
    }

    pub fn with_action(
        msg: impl Into<String>,
        duration_ms: u64,
        label: impl Into<String>,
        intent: HudIntent,
    ) -> Self {
        Self {
            message: msg.into(),
            action: Some(HudAction {
                label: label.into(),
                intent,
            }),
            shown_at: Instant::now(),
            duration_ms,
        }
    }

    pub fn expired(&self) -> bool {
        self.shown_at.elapsed().as_millis() as u64 > self.duration_ms
    }
}

const PANIC_WINDOW: Duration = Duration::from_secs(10);
const PANIC_THRESHOLD: usize = 5;
const NAV_DEBOUNCE: Duration = Duration::from_millis(120);

impl MergeFoxApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut config = Config::load();
        config.prune_recents();
        ui::fonts::ensure_language_fonts(&cc.egui_ctx, config.ui_language.resolved());
        ui::theme::apply(&cc.egui_ctx, &config.theme);

        // Secret store uses whatever backend the user chose in Settings.
        // Wrapped in `Arc` so background tasks (AI generation, PAT
        // verification, OAuth polling) can clone a handle without
        // threading `&self` through everywhere. We also install it as
        // the process-wide default so the existing `providers::pat::*`
        // and `ai::config::{load,save,delete}_api_key` module functions
        // can route through it from deep inside closures without new
        // parameters.
        let secret_store = Arc::new(crate::secrets::SecretStore::new(
            crate::secrets::default_file_path(),
        ));
        secret_store.clone().install_global();

        Self {
            view: View::Welcome(WelcomeState::default()),
            config,
            egui_ctx: cc.egui_ctx.clone(),
            secret_store,
            git_capability: crate::git::probe_git_capability(),
            last_error: None,
            hud: None,
            notifications: crate::ui::notifications::NotificationCenter::default(),
            startup_notified: false,
            blame: crate::ui::blame::BlameState::default(),
            bisect_ui: crate::ui::bisect::BisectUiState::default(),
            image_loaders_installed: false,
            nav_history: VecDeque::new(),
            panic_modal_open: false,
            commit_modal_open: false,
            commit_modal: None,
            pending_prompt: None,
            columns_popover_open: false,
            activity_log_open: false,
            reflog_open: false,
            shortcuts_open: false,
            palette_open: false,
            palette_query: String::new(),
            palette_selected: 0,
            settings_open: false,
            settings_modal: None,
            publish_remote_modal: None,
            ai_test_task: None,
            provider_oauth_start_task: None,
            provider_oauth_poll_task: None,
            commit_ai_task: None,
            forge_refresh_task: None,
            forge_create_pr_task: None,
            forge_create_issue_task: None,
        }
    }

    pub fn refresh_git_capability(&mut self) {
        self.git_capability = crate::git::probe_git_capability();
    }

    /// Fire a one-shot startup notification when the system `git`
    /// binary isn't on PATH. Without this the user only finds out
    /// during their first write op, which is way too late for a Git
    /// GUI — every fetch / commit / push silently fails until they
    /// figure out they need to install git separately.
    ///
    /// We don't bundle git in the installer because:
    ///   * macOS: Xcode CLT ships it; most devs already have it.
    ///   * Windows: Git for Windows is ~50 MB; bundling would double
    ///     the mergeFox installer size and the `git` licence (GPLv2)
    ///     gets weird when combined with our Apache-2.0 code. Users
    ///     should install git themselves via the official MSI.
    ///   * Linux: always available via the package manager; our
    ///     `.deb` / `.rpm` list it as a dependency.
    pub fn notify_git_probe_on_startup(&mut self) {
        if !self.git_capability.is_available() {
            let detail = self.git_capability.install_guidance().to_string();
            self.notify_err_with_detail(
                "System `git` not found on PATH — mergeFox needs git for all write operations.",
                detail,
            );
        }
    }

    pub fn git_missing_message(&self, action: &str) -> String {
        format!(
            "{action} needs the system git CLI, but `git` is not available. {}",
            self.git_capability.install_guidance()
        )
    }

    pub fn ensure_git_cli_for(&mut self, action: &str) -> bool {
        if !self.git_capability.is_available() {
            self.refresh_git_capability();
        }
        if self.git_capability.is_available() {
            true
        } else {
            self.last_error = Some(self.git_missing_message(action));
            false
        }
    }

    pub fn set_git_error(&mut self, action: &str, err: impl std::fmt::Display) {
        let diagnostic = crate::git::classify_git_error(err.to_string());
        if matches!(diagnostic.kind, crate::git::GitErrorKind::MissingBinary) {
            self.refresh_git_capability();
        }
        self.last_error = Some(diagnostic.user_message(action));
    }

    pub fn ensure_image_loaders(&mut self, ctx: &egui::Context) {
        if self.image_loaders_installed {
            return;
        }
        egui_extras::install_image_loaders(ctx);
        self.image_loaders_installed = true;
    }

    /// Open a repository asynchronously.
    ///
    /// The slow part of opening a repo is (1) `Repository::discover`
    /// loading packed-refs (for kernel-scale repos this can be hundreds
    /// of milliseconds to seconds) and (2) `build_graph` walking up to
    /// `MAX_GRAPH_COMMITS`. Doing both on the UI thread froze the window.
    ///
    /// Instead: spawn a worker that does those two; show an
    /// `OpeningRepo` loading view with a status label the worker writes
    /// into; when it's done we receive the `CommitGraph` over a channel
    /// and re-open the `Repo` on the main thread (cheap now because
    /// git's refs / pack indexes are already in the OS page cache).
    /// Run `git init` on `path` (creating the directory first if needed),
    /// then route through `open_repo` to show it as a fresh workspace tab.
    /// Idempotent — running on a directory that's already a repo is a
    /// no-op on git's side and we just open it.
    pub fn init_repo(&mut self, path: &Path) {
        if let Err(e) = std::fs::create_dir_all(path) {
            self.last_error = Some(format!("create {}: {e:#}", path.display()));
            return;
        }
        if !self.ensure_git_cli_for("Initializing a repository") {
            return;
        }
        if let Err(e) = crate::git::cli::run(path, ["init"]) {
            self.set_git_error("Initializing a repository", e);
            return;
        }
        self.open_repo(path);
    }

    pub fn open_repo(&mut self, path: &Path) {
        // If the user is already in a workspace (clicking Open Recent
        // from a second tab, for instance), move the existing
        // WorkspaceTabs aside so we can append the new tab to it when
        // the async open finishes. Without this, every open from within
        // a workspace silently discards all other tabs.
        //
        // Also: if the same repo is already open, just focus it and
        // return — no need to reopen.
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let mut preserved_tabs: Option<WorkspaceTabs> = None;
        if let View::Workspace(tabs) = &mut self.view {
            if let Some(idx) = tabs.tabs.iter().position(|t| {
                t.repo
                    .path()
                    .canonicalize()
                    .unwrap_or_else(|_| t.repo.path().to_path_buf())
                    == canonical
            }) {
                tabs.active = idx;
                tabs.launcher_active = false;
                return;
            }
            // Take ownership by swapping in a placeholder — we put the
            // tabs back in OpeningRepoState below.
            let taken = std::mem::replace(
                tabs,
                WorkspaceTabs {
                    tabs: Vec::new(),
                    active: 0,
                    launcher_tab: None,
                    launcher_active: false,
                },
            );
            preserved_tabs = Some(taken);
        }

        let (tx, rx) = std::sync::mpsc::channel::<OpenOutcome>();
        let label = Arc::new(std::sync::Mutex::new("Opening repository…".to_string()));
        let label_worker = label.clone();
        let path_owned = path.to_path_buf();

        std::thread::spawn(move || {
            // Stage 1: Repository::discover (the expensive packed-refs load).
            let repo = match Repo::open(&path_owned) {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(OpenOutcome::Err(format!("{e:#}")));
                    return;
                }
            };

            // Stage 2: graph walk. CommitGraph is `Send` so we can ship
            // it back; `Repo` is `!Send` because `gix::Repository` is,
            // so we drop it here and re-open on the main thread.
            *label_worker.lock().unwrap() = "Building commit graph…".to_string();
            let graph = match repo.build_graph(GraphScope::AllLocal) {
                Ok(g) => g,
                Err(e) => {
                    let _ = tx.send(OpenOutcome::Err(format!("graph: {e:#}")));
                    return;
                }
            };
            drop(repo);

            let _ = tx.send(OpenOutcome::Ok {
                path: path_owned,
                graph,
            });
        });

        self.view = View::OpeningRepo(OpeningRepoState {
            path: path.to_path_buf(),
            started_at: Instant::now(),
            label,
            rx,
            preserved_tabs,
        });
    }

    /// Drain the async-open channel each frame. On success, switch to
    /// a Workspace view with the freshly-built graph; on failure, fall
    /// back to Welcome (or launcher tab) with the same stale-path
    /// diagnostics the old sync path had.
    pub fn poll_opening_repo(&mut self) {
        // Peek: drain the channel with a single `try_recv` on the shared
        // borrow. Receiver::try_recv is `&self` so no ownership issue.
        // If nothing is waiting, we're done for this frame.
        let outcome = match &self.view {
            View::OpeningRepo(state) => match state.rx.try_recv() {
                Ok(o) => o,
                Err(_) => return,
            },
            _ => return,
        };
        // Take ownership of the state now that we have the outcome;
        // `preserved_tabs` needs to be consumed regardless of branch.
        let state = match std::mem::replace(&mut self.view, View::Welcome(WelcomeState::default()))
        {
            View::OpeningRepo(s) => s,
            other => {
                self.view = other;
                return;
            }
        };
        let original_path = state.path.clone();
        let preserved_tabs = state.preserved_tabs;

        match outcome {
            OpenOutcome::Ok { path, graph } => {
                self.finalize_opened_repo(path, graph, preserved_tabs);
            }
            OpenOutcome::Err(e) => {
                // Classify + prune stale Recents + surface diagnostics.
                let kind = classify_stale_path(&original_path);
                self.config.recents.retain(|r| r.path != original_path);
                let _ = self.config.save();
                self.last_error = Some(match kind {
                    StalePathKind::Missing => format!(
                        "open {}: folder no longer exists; removed from Recents",
                        original_path.display(),
                    ),
                    StalePathKind::PartialClone => format!(
                        "open {}: looks like a failed clone (no HEAD). Delete the \
                         folder and re-clone, or drag a different repo here. \
                         Removed from Recents.",
                        original_path.display(),
                    ),
                    StalePathKind::Other => format!("open {}: {e}", original_path.display()),
                });
                // Restore whatever the user had before the failed open —
                // existing workspace tabs if they had them, otherwise a
                // fresh Welcome view.
                self.view = match preserved_tabs {
                    Some(tabs) => View::Workspace(tabs),
                    None => View::Welcome(WelcomeState::default()),
                };
            }
        }
    }

    fn finalize_opened_repo(
        &mut self,
        repo_path: PathBuf,
        graph: crate::git::CommitGraph,
        preserved_tabs: Option<WorkspaceTabs>,
    ) {
        // Re-open the repo on the main thread. This is cheap after the
        // worker primed the OS page cache with packed-refs + index files.
        let repo = match Repo::open(&repo_path) {
            Ok(r) => r,
            Err(e) => {
                self.last_error = Some(format!("re-open: {e:#}"));
                // Restore preserved tabs if any — don't penalise the
                // user for our re-open failing by killing their other
                // tabs.
                self.view = match preserved_tabs {
                    Some(tabs) => View::Workspace(tabs),
                    None => View::Welcome(WelcomeState::default()),
                };
                return;
            }
        };

        self.config.touch_recent(repo_path.clone());
        let _ = self.config.save();

        let scope = GraphScope::AllLocal;
        let graph_view = Some(GraphView::new(Arc::new(graph)));
        let journal = Journal::load_or_init(repo.git_dir()).ok();
        let tab_title = repo_path
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| repo_path.display().to_string());
        let lfs_scan = spawn_lfs_scan(&repo_path);

        let new_ws = WorkspaceState {
            repo,
            selected_branch: None,
            graph_scope: scope,
            graph_view,
            journal,
            active_job: None,
            selected_commit: None,
            current_diff: None,
            selected_file_idx: None,
            selected_file_view: SelectedFileView::Diff,
            file_list_tree: false,
            selected_image_cache: None,
            snapshot_cache: None,
            pending_image_evictions: Vec::new(),
            selected_conflict: None,
            conflict_editor_path: None,
            conflict_editor_text: String::new(),
            rebase_modal: None,
            rebase_session: None,
            column_prefs: ColumnPrefs::default(),
            tab_title,
            forge: ForgeState::default(),
            lfs_scan,
            nav_task: None,
            diff_task: None,
            pending_diff_oid: None,
            diff_cache: DiffCache::default(),
            graph_task: None,
            repo_ui_cache: None,
            last_working_tree_poll: Instant::now(),
            last_repo_external_poll: Instant::now(),
            repo_external_snapshot: None,
            working_tree_expanded: false,
            selected_working_file: None,
            working_file_diff: None,
            selected_working_tree: false,
        };

        // If we came from an existing workspace, append the new tab
        // and focus it — preserving all the user's other open tabs.
        // Otherwise spin up a fresh WorkspaceTabs (first-repo case).
        self.view = match preserved_tabs {
            Some(mut tabs) => {
                tabs.tabs.push(new_ws);
                tabs.active = tabs.tabs.len() - 1;
                tabs.launcher_active = false;
                View::Workspace(tabs)
            }
            None => View::Workspace(WorkspaceTabs {
                tabs: vec![new_ws],
                active: 0,
                launcher_tab: None,
                launcher_active: false,
            }),
        };
        self.release_inactive_tab_caches();
        self.restore_active_tab_cache();
        self.ensure_active_forge_loaded();
        self.ensure_repo_ui_cache();
    }

    /// Focus a tab by index. No-op if out of range.
    pub fn focus_tab(&mut self, idx: usize) {
        if let View::Workspace(tabs) = &mut self.view {
            if idx < tabs.tabs.len() {
                tabs.active = idx;
                tabs.launcher_active = false;
            }
        }
        self.release_inactive_tab_caches();
        self.restore_active_tab_cache();
        self.ensure_active_forge_loaded();
        self.ensure_repo_ui_cache();
    }

    /// Close a tab. If it was the last one we drop back to the Welcome screen.
    pub fn close_tab(&mut self, idx: usize) {
        if let View::Workspace(tabs) = &mut self.view {
            if idx >= tabs.tabs.len() {
                return;
            }
            tabs.tabs.remove(idx);
            if tabs.tabs.is_empty() {
                self.view = View::Welcome(tabs.launcher_tab.take().unwrap_or_default());
                return;
            }
            if tabs.active >= tabs.tabs.len() {
                tabs.active = tabs.tabs.len() - 1;
            } else if idx <= tabs.active && tabs.active > 0 {
                tabs.active -= 1;
            }
        }
        self.release_inactive_tab_caches();
        self.restore_active_tab_cache();
        self.ensure_active_forge_loaded();
        self.ensure_repo_ui_cache();
    }

    pub fn close_active_tab(&mut self) {
        match &self.view {
            View::Workspace(tabs) if tabs.launcher_active => {
                self.close_launcher_tab();
                return;
            }
            View::Workspace(tabs) => {
                self.close_tab(tabs.active);
                return;
            }
            View::OpeningRepo(_) => {
                // Nothing to close while we're mid-open; the user
                // either waits for it to finish or closes the window.
                return;
            }
            View::Welcome(_) => {}
        }
    }

    /// Cycle to the next tab, wrapping.
    pub fn next_tab(&mut self) {
        if let View::Workspace(tabs) = &mut self.view {
            let has_launcher = tabs.launcher_tab.is_some();
            let total = tabs.tabs.len() + usize::from(has_launcher);
            if total == 0 {
                return;
            }
            let current = if tabs.launcher_active {
                tabs.tabs.len()
            } else {
                tabs.active.min(tabs.tabs.len().saturating_sub(1))
            };
            let next = (current + 1) % total;
            if has_launcher && next == tabs.tabs.len() {
                tabs.launcher_active = true;
            } else {
                tabs.active = next;
                tabs.launcher_active = false;
            }
        }
        self.release_inactive_tab_caches();
        self.restore_active_tab_cache();
        self.ensure_active_forge_loaded();
        self.ensure_repo_ui_cache();
    }

    /// Cycle to the previous tab, wrapping.
    pub fn prev_tab(&mut self) {
        if let View::Workspace(tabs) = &mut self.view {
            let has_launcher = tabs.launcher_tab.is_some();
            let total = tabs.tabs.len() + usize::from(has_launcher);
            if total == 0 {
                return;
            }
            let current = if tabs.launcher_active {
                tabs.tabs.len()
            } else {
                tabs.active.min(tabs.tabs.len().saturating_sub(1))
            };
            let prev = (current + total - 1) % total;
            if has_launcher && prev == tabs.tabs.len() {
                tabs.launcher_active = true;
            } else {
                tabs.active = prev;
                tabs.launcher_active = false;
            }
        }
        self.release_inactive_tab_caches();
        self.restore_active_tab_cache();
        self.ensure_active_forge_loaded();
        self.ensure_repo_ui_cache();
    }

    pub fn rebuild_graph(&mut self, scope: GraphScope) {
        let View::Workspace(tabs) = &mut self.view else {
            return;
        };
        let ws = tabs.current_mut();
        ws.graph_scope = scope;
        spawn_graph_task(ws, scope, &self.egui_ctx);
        // Repo state changed (commit / rebase / checkout / …), so the
        // cached branch / status / stash lists are stale. Refresh them
        // here, once, rather than having the sidebar re-scan every
        // single frame until the next op.
        self.refresh_repo_ui_cache();
    }

    /// Populate the repo UI cache if it's missing (first activation of
    /// this tab, or after `release_inactive_tab_caches` dropped it).
    /// Cheap when the cache is already warm.
    pub fn ensure_repo_ui_cache(&mut self) {
        let needs_refresh = match &self.view {
            View::Workspace(tabs) if !tabs.launcher_active => tabs
                .tabs
                .get(tabs.active)
                .map(|ws| ws.repo_ui_cache.is_none())
                .unwrap_or(false),
            _ => false,
        };
        if needs_refresh {
            self.refresh_repo_ui_cache();
        }
    }

    /// Recompute branch list, stash list, working-tree status and the
    /// remote list for the active tab. Called on tab open, after every
    /// git op (via `rebuild_graph`), and when the user explicitly
    /// refreshes. The sidebar / top-bar / main-panel read from the
    /// cached snapshot instead of re-hitting gix / git each paint.
    pub fn refresh_repo_ui_cache(&mut self) {
        let View::Workspace(tabs) = &mut self.view else {
            return;
        };
        if tabs.launcher_active {
            return;
        }
        let ws = tabs.current_mut();
        let (branch_error, mut branches) = match ws.repo.list_branches(true) {
            Ok(branches) => (None, branches),
            Err(err) => (Some(format!("error: {err}")), Vec::new()),
        };
        // Ahead/behind counts vs. upstream for the sidebar pill. Cheap
        // per branch (one `rev-list --count`), runs only when the
        // cache refresh fires, and `populate_tracking_counts` already
        // no-ops the branches that have no upstream so unconfigured
        // remotes cost nothing.
        ws.repo.populate_tracking_counts(&mut branches);
        let head_has_upstream = branches
            .iter()
            .find(|branch| branch.is_head && !branch.is_remote)
            .and_then(|branch| branch.upstream.as_ref())
            .is_some();
        let (stash_error, stashes) = match crate::git::ops::stash_list(ws.repo.path()) {
            Ok(entries) => (None, Some(entries)),
            Err(err) => (
                Some(
                    crate::git::classify_git_error(format!("{err:#}"))
                        .user_message("Reading stash list"),
                ),
                None,
            ),
        };
        let (working_error, working) = match crate::git::ops::status_entries(ws.repo.path()) {
            Ok(entries) => (None, Some(entries)),
            Err(err) => (
                Some(
                    crate::git::classify_git_error(format!("{err:#}"))
                        .user_message("Reading working tree status"),
                ),
                None,
            ),
        };
        let remotes = ws
            .repo
            .list_remotes()
            .unwrap_or_default()
            .into_iter()
            .filter(|remote| remote.fetch_url.is_some() || remote.push_url.is_some())
            .map(|r| r.name)
            .collect::<Vec<_>>();
        // ahead/behind: `git rev-list --count --left-right HEAD...@{upstream}`
        // Returns "A\tB" where A = ahead, B = behind. Fails cleanly when
        // there's no upstream (returns 0, 0).
        let (ahead, behind, tracking_error) = match crate::git::cli::run_line(
            ws.repo.path(),
            ["rev-list", "--count", "--left-right", "HEAD...@{upstream}"],
        ) {
            Ok(line) => {
                let parts: Vec<&str> = line.trim().split('\t').collect();
                if parts.len() == 2 {
                    (
                        parts[0].parse::<usize>().unwrap_or(0),
                        parts[1].parse::<usize>().unwrap_or(0),
                        None,
                    )
                } else {
                    (0, 0, Some("Unable to compute ahead/behind counts.".into()))
                }
            }
            Err(_) if !head_has_upstream => (0, 0, None),
            Err(err) => (
                0,
                0,
                Some(
                    crate::git::classify_git_error(format!("{err:#}"))
                        .user_message("Computing upstream ahead/behind counts"),
                ),
            ),
        };

        ws.repo_ui_cache = Some(RepoUiCache {
            branches,
            branch_error,
            stashes,
            stash_error,
            working,
            working_error,
            remotes,
            ahead,
            behind,
            tracking_error,
        });
        ws.last_working_tree_poll = Instant::now();
        ws.last_repo_external_poll = Instant::now();
        ws.repo_external_snapshot = Some(capture_repo_external_snapshot(&ws.repo));
    }

    /// Poll `git status` on a low-frequency timer so out-of-band edits
    /// (editor save, codegen, terminal commands) update the cached
    /// working-tree counters without requiring a manual refresh.
    fn poll_working_tree_changes(&mut self) {
        let mut should_refresh_cache = false;
        let mut should_rebuild_graph = false;
        let mut rebuild_scope = GraphScope::CurrentBranch;
        let mut show_external_change_hud = false;

        {
            let View::Workspace(tabs) = &mut self.view else {
                return;
            };
            if tabs.launcher_active {
                return;
            }

            let ws = tabs.current_mut();
            let poll_working = ws.last_working_tree_poll.elapsed() >= WORKING_TREE_POLL_INTERVAL;
            let poll_external = ws.last_repo_external_poll.elapsed() >= WORKING_TREE_POLL_INTERVAL;
            if !poll_working && !poll_external {
                return;
            }

            if poll_working {
                ws.last_working_tree_poll = Instant::now();
                if let Some(cache) = ws.repo_ui_cache.as_mut() {
                    match crate::git::ops::status_entries(ws.repo.path()) {
                        Ok(working) => {
                            cache.working_error = None;
                            if cache.working.as_ref() != Some(&working) {
                                cache.working = Some(working.clone());
                                if let Some(selected_path) = ws.selected_working_file.clone() {
                                    let still_present =
                                        working.iter().any(|entry| entry.path == selected_path);
                                    if still_present {
                                        // Force diff/image recomputation for the next paint so
                                        // the detail pane reflects the updated file contents.
                                        ws.working_file_diff = None;
                                        ws.set_image_cache(None);
                                    } else {
                                        ws.selected_working_file = None;
                                        ws.working_file_diff = None;
                                        ws.set_image_cache(None);
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            cache.working = None;
                            cache.working_error = Some(
                                crate::git::classify_git_error(format!("{err:#}"))
                                    .user_message("Reading working tree status"),
                            );
                        }
                    }
                }
            }

            if poll_external {
                ws.last_repo_external_poll = Instant::now();
                let snapshot = capture_repo_external_snapshot(&ws.repo);
                if let Some(previous) = ws.repo_external_snapshot.as_ref() {
                    if previous != &snapshot {
                        should_refresh_cache = true;
                        should_rebuild_graph = snapshot.requires_graph_refresh(previous);
                        show_external_change_hud = true;
                        rebuild_scope = ws.graph_scope;
                    }
                }
                ws.repo_external_snapshot = Some(snapshot);
            }
        }

        if should_rebuild_graph {
            self.rebuild_graph(rebuild_scope);
        } else if should_refresh_cache {
            self.refresh_repo_ui_cache();
        }
        if show_external_change_hud {
            self.hud = Some(Hud::new(
                "Repository changed outside MergeFox — refreshed",
                1800,
            ));
        }
    }

    pub fn go_home(&mut self) {
        match &mut self.view {
            View::Workspace(tabs) => {
                if tabs.launcher_tab.is_none() {
                    tabs.launcher_tab = Some(WelcomeState::default());
                }
                tabs.launcher_active = true;
            }
            View::Welcome(state) => {
                *state = WelcomeState::default();
            }
            View::OpeningRepo(_) => {
                // Ignore — the open will finish and hand the user a
                // workspace view; trying to "go home" mid-open is not
                // a supported escape hatch.
            }
        }
        self.release_inactive_tab_caches();
    }

    pub fn open_new_tab(&mut self) {
        match &mut self.view {
            View::Workspace(tabs) => {
                if tabs.launcher_tab.is_none() {
                    tabs.launcher_tab = Some(WelcomeState::default());
                }
                tabs.launcher_active = true;
            }
            View::Welcome(state) => {
                *state = WelcomeState::default();
            }
            View::OpeningRepo(_) => {
                // Ignore — the open will finish and hand the user a
                // workspace view; trying to "go home" mid-open is not
                // a supported escape hatch.
            }
        }
        self.release_inactive_tab_caches();
    }

    pub fn close_launcher_tab(&mut self) {
        if let View::Workspace(tabs) = &mut self.view {
            let has_clone = tabs
                .launcher_tab
                .as_ref()
                .and_then(|state| state.clone.as_ref())
                .is_some();
            if !has_clone {
                tabs.launcher_tab = None;
            }
            tabs.launcher_active = false;
        }
        self.restore_active_tab_cache();
        self.ensure_active_forge_loaded();
    }

    pub fn active_welcome_state(&self) -> Option<&WelcomeState> {
        match &self.view {
            View::Welcome(state) => Some(state),
            View::Workspace(tabs) if tabs.launcher_active => tabs.launcher_tab.as_ref(),
            _ => None,
        }
    }

    pub fn active_welcome_state_mut(&mut self) -> Option<&mut WelcomeState> {
        match &mut self.view {
            View::Welcome(state) => Some(state),
            View::Workspace(tabs) if tabs.launcher_active => tabs.launcher_tab.as_mut(),
            _ => None,
        }
    }

    pub fn background_welcome_state(&self) -> Option<&WelcomeState> {
        match &self.view {
            View::Welcome(state) => Some(state),
            View::Workspace(tabs) => tabs.launcher_tab.as_ref(),
            View::OpeningRepo(_) => None,
        }
    }

    pub fn background_welcome_state_mut(&mut self) -> Option<&mut WelcomeState> {
        match &mut self.view {
            View::Welcome(state) => Some(state),
            View::Workspace(tabs) => tabs.launcher_tab.as_mut(),
            View::OpeningRepo(_) => None,
        }
    }

    pub fn clone_in_progress(&self) -> bool {
        match &self.view {
            View::Welcome(state) => state.clone.is_some(),
            View::Workspace(tabs) => tabs
                .launcher_tab
                .as_ref()
                .and_then(|state| state.clone.as_ref())
                .is_some(),
            View::OpeningRepo(_) => false,
        }
    }

    pub fn remote_repo_refresh_in_progress(&self) -> bool {
        self.background_welcome_state().is_some_and(|state| {
            state.remote_repos.task.is_some()
                || state.remote_repos.create_repo.owners_task.is_some()
                || state.remote_repos.create_repo.create_task.is_some()
        })
    }

    pub fn poll_clone_jobs(&mut self) {
        let result = match &mut self.view {
            View::Welcome(state) => {
                let result = state.clone.as_ref().and_then(|handle| handle.poll());
                if result.is_some() {
                    state.clone = None;
                }
                result
            }
            View::Workspace(tabs) => {
                let Some(launcher) = tabs.launcher_tab.as_mut() else {
                    return;
                };
                let result = launcher.clone.as_ref().and_then(|handle| handle.poll());
                if result.is_some() {
                    launcher.clone = None;
                }
                result
            }
            View::OpeningRepo(_) => return,
        };

        let Some(result) = result else {
            return;
        };

        match result {
            Ok(path) => self.open_repo(&path),
            Err(err) => self.set_git_error("Cloning a repository", err),
        }
    }

    pub fn repo_browser_accounts(&self) -> Vec<providers::ProviderAccount> {
        self.config
            .provider_accounts
            .iter()
            .filter(|account| {
                providers::pat::load_pat(&account.id)
                    .ok()
                    .flatten()
                    .is_some()
            })
            .cloned()
            .collect()
    }

    pub fn open_publish_remote_modal(&mut self, branch: Option<String>) {
        let (repo_path, branch, repository_name, preferred_account) = {
            let View::Workspace(tabs) = &self.view else {
                return;
            };
            if tabs.launcher_active {
                return;
            }
            let ws = tabs.current();
            let branch = match branch.or_else(|| ws.repo.head_name()) {
                Some(branch) => branch,
                None => {
                    self.last_error =
                        Some("check out a local branch before publishing".to_string());
                    return;
                }
            };
            let repo_path = ws.repo.path().to_path_buf();
            let repository_name = repo_path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
                .unwrap_or_else(|| "repository".to_string());
            let preferred_account = self
                .config
                .repo_settings_for(ws.repo.path())
                .provider_account;
            (repo_path, branch, repository_name, preferred_account)
        };

        let connected_accounts = self.repo_browser_accounts();
        let selected_account = preferred_account
            .as_deref()
            .and_then(|slug| {
                connected_accounts
                    .iter()
                    .find(|account| account.id.slug() == slug)
                    .map(|account| account.id.clone())
            })
            .or_else(|| connected_accounts.first().map(|account| account.id.clone()));

        self.publish_remote_modal = Some(PublishRemoteModal {
            repo_path,
            branch,
            selected_account: selected_account.clone(),
            owners: Vec::new(),
            owners_task: None,
            create_task: None,
            selected_owner: None,
            remote_name: "origin".to_string(),
            repository_name,
            description: String::new(),
            private: true,
            last_error: None,
        });

        if let Some(account_id) = selected_account {
            if let Some(account) = connected_accounts
                .iter()
                .find(|account| account.id == account_id)
                .cloned()
            {
                self.load_publish_remote_owners(&account);
            }
        }
    }

    pub fn load_publish_remote_owners(&mut self, account: &providers::ProviderAccount) {
        let token = match providers::pat::load_pat(&account.id) {
            Ok(Some(token)) => token,
            Ok(None) => {
                if let Some(modal) = self.publish_remote_modal.as_mut() {
                    modal.owners_task = None;
                    modal.owners.clear();
                    modal.last_error =
                        Some("account token is missing from the OS keychain".to_string());
                }
                return;
            }
            Err(err) => {
                if let Some(modal) = self.publish_remote_modal.as_mut() {
                    modal.owners_task = None;
                    modal.owners.clear();
                    modal.last_error = Some(format!("keyring: {err:#}"));
                }
                return;
            }
        };

        let kind = account.id.kind.clone();
        let client = providers::default_http_client();
        let task = providers::runtime::ProviderTask::spawn(async move {
            let provider = providers::build(&kind).await;
            provider.list_repository_owners(&client, &token).await
        });

        if let Some(modal) = self.publish_remote_modal.as_mut() {
            modal.selected_account = Some(account.id.clone());
            modal.owners_task = Some(task);
            modal.last_error = None;
        }
    }

    pub fn create_publish_remote(
        &mut self,
        account: &providers::ProviderAccount,
        draft: providers::CreateRepositoryDraft,
    ) {
        let token = match providers::pat::load_pat(&account.id) {
            Ok(Some(token)) => token,
            Ok(None) => {
                if let Some(modal) = self.publish_remote_modal.as_mut() {
                    modal.create_task = None;
                    modal.last_error =
                        Some("account token is missing from the OS keychain".to_string());
                }
                return;
            }
            Err(err) => {
                if let Some(modal) = self.publish_remote_modal.as_mut() {
                    modal.create_task = None;
                    modal.last_error = Some(format!("keyring: {err:#}"));
                }
                return;
            }
        };

        let kind = account.id.kind.clone();
        let client = providers::default_http_client();
        let task = providers::runtime::ProviderTask::spawn(async move {
            let provider = providers::build(&kind).await;
            provider.create_repository(&client, &token, &draft).await
        });

        if let Some(modal) = self.publish_remote_modal.as_mut() {
            modal.selected_account = Some(account.id.clone());
            modal.create_task = Some(task);
            modal.last_error = None;
        }
    }

    pub fn refresh_remote_repositories(&mut self, account: &providers::ProviderAccount) {
        let token = match providers::pat::load_pat(&account.id) {
            Ok(Some(token)) => token,
            Ok(None) => {
                if let Some(state) = self.background_welcome_state_mut() {
                    state.remote_repos.task = None;
                    state.remote_repos.repos.clear();
                    state.remote_repos.loaded_once = true;
                    state.remote_repos.last_error =
                        Some("account token is missing from the OS keychain".to_string());
                }
                return;
            }
            Err(err) => {
                if let Some(state) = self.background_welcome_state_mut() {
                    state.remote_repos.task = None;
                    state.remote_repos.repos.clear();
                    state.remote_repos.loaded_once = true;
                    state.remote_repos.last_error = Some(format!("keyring: {err:#}"));
                }
                return;
            }
        };

        let kind = account.id.kind.clone();
        let client = providers::default_http_client();
        let task = providers::runtime::ProviderTask::spawn(async move {
            let provider = providers::build(&kind).await;
            provider.list_accessible_repositories(&client, &token).await
        });

        if let Some(state) = self.background_welcome_state_mut() {
            state.remote_repos.selected_account = Some(account.id.clone());
            state.remote_repos.task = Some(task);
            state.remote_repos.last_error = None;
            state.remote_repos.repos.clear();
            state.remote_repos.loaded_once = false;
        }
    }

    pub fn load_remote_repo_owners(&mut self, account: &providers::ProviderAccount) {
        let token = match providers::pat::load_pat(&account.id) {
            Ok(Some(token)) => token,
            Ok(None) => {
                if let Some(state) = self.background_welcome_state_mut() {
                    state.remote_repos.create_repo.owners_task = None;
                    state.remote_repos.create_repo.owners.clear();
                    state.remote_repos.create_repo.last_error =
                        Some("account token is missing from the OS keychain".to_string());
                }
                return;
            }
            Err(err) => {
                if let Some(state) = self.background_welcome_state_mut() {
                    state.remote_repos.create_repo.owners_task = None;
                    state.remote_repos.create_repo.owners.clear();
                    state.remote_repos.create_repo.last_error = Some(format!("keyring: {err:#}"));
                }
                return;
            }
        };

        let kind = account.id.kind.clone();
        let client = providers::default_http_client();
        let task = providers::runtime::ProviderTask::spawn(async move {
            let provider = providers::build(&kind).await;
            provider.list_repository_owners(&client, &token).await
        });

        if let Some(state) = self.background_welcome_state_mut() {
            state.remote_repos.selected_account = Some(account.id.clone());
            state.remote_repos.create_repo.owners_task = Some(task);
            state.remote_repos.create_repo.last_error = None;
            state.remote_repos.create_repo.last_created = None;
        }
    }

    pub fn create_remote_repository(
        &mut self,
        account: &providers::ProviderAccount,
        draft: providers::CreateRepositoryDraft,
    ) {
        let token = match providers::pat::load_pat(&account.id) {
            Ok(Some(token)) => token,
            Ok(None) => {
                if let Some(state) = self.background_welcome_state_mut() {
                    state.remote_repos.create_repo.create_task = None;
                    state.remote_repos.create_repo.last_error =
                        Some("account token is missing from the OS keychain".to_string());
                }
                return;
            }
            Err(err) => {
                if let Some(state) = self.background_welcome_state_mut() {
                    state.remote_repos.create_repo.create_task = None;
                    state.remote_repos.create_repo.last_error = Some(format!("keyring: {err:#}"));
                }
                return;
            }
        };

        let kind = account.id.kind.clone();
        let client = providers::default_http_client();
        let task = providers::runtime::ProviderTask::spawn(async move {
            let provider = providers::build(&kind).await;
            provider.create_repository(&client, &token, &draft).await
        });

        if let Some(state) = self.background_welcome_state_mut() {
            state.remote_repos.selected_account = Some(account.id.clone());
            state.remote_repos.create_repo.create_task = Some(task);
            state.remote_repos.create_repo.last_error = None;
            state.remote_repos.create_repo.last_created = None;
        }
    }

    pub fn poll_remote_repo_jobs(&mut self) {
        self.poll_remote_repo_list_task();
        self.poll_remote_repo_owner_task();
        self.poll_remote_repo_create_task();
    }

    fn poll_remote_repo_list_task(&mut self) {
        let result = {
            let Some(state) = self.background_welcome_state_mut() else {
                return;
            };
            let result = state
                .remote_repos
                .task
                .as_mut()
                .and_then(|task| task.poll());
            if result.is_some() {
                state.remote_repos.task = None;
            }
            result
        };

        let Some(result) = result else {
            return;
        };

        if let Some(state) = self.background_welcome_state_mut() {
            state.remote_repos.loaded_once = true;
            match result {
                Ok(repos) => {
                    state.remote_repos.repos = repos;
                    state.remote_repos.last_error = None;
                }
                Err(err) => {
                    state.remote_repos.repos.clear();
                    state.remote_repos.last_error = Some(err.to_string());
                }
            }
        }
    }

    fn poll_remote_repo_owner_task(&mut self) {
        let result = {
            let Some(state) = self.background_welcome_state_mut() else {
                return;
            };
            let result = state
                .remote_repos
                .create_repo
                .owners_task
                .as_mut()
                .and_then(|task| task.poll());
            if result.is_some() {
                state.remote_repos.create_repo.owners_task = None;
            }
            result
        };

        let Some(result) = result else {
            return;
        };

        if let Some(state) = self.background_welcome_state_mut() {
            match result {
                Ok(owners) => {
                    let selected = state.remote_repos.create_repo.selected_owner.clone();
                    state.remote_repos.create_repo.owners = owners;
                    let selected_still_exists = selected.as_ref().is_some_and(|login| {
                        state
                            .remote_repos
                            .create_repo
                            .owners
                            .iter()
                            .any(|owner| owner.login == *login)
                    });
                    if !selected_still_exists {
                        state.remote_repos.create_repo.selected_owner = state
                            .remote_repos
                            .create_repo
                            .owners
                            .first()
                            .map(|owner| owner.login.clone());
                    }
                    state.remote_repos.create_repo.last_error = None;
                }
                Err(err) => {
                    state.remote_repos.create_repo.owners.clear();
                    state.remote_repos.create_repo.last_error = Some(err.to_string());
                }
            }
        }
    }

    fn poll_remote_repo_create_task(&mut self) {
        let result = {
            let Some(state) = self.background_welcome_state_mut() else {
                return;
            };
            let result = state
                .remote_repos
                .create_repo
                .create_task
                .as_mut()
                .and_then(|task| task.poll());
            if result.is_some() {
                state.remote_repos.create_repo.create_task = None;
            }
            result
        };

        let Some(result) = result else {
            return;
        };

        let mut refresh_account = None;
        match result {
            Ok(created) => {
                if let Some(state) = self.background_welcome_state_mut() {
                    state.remote_repos.loaded_once = true;
                    if !state
                        .remote_repos
                        .repos
                        .iter()
                        .any(|repo| repo.owner == created.owner && repo.repo == created.repo)
                    {
                        state.remote_repos.repos.insert(
                            0,
                            providers::RemoteRepoSummary {
                                owner: created.owner.clone(),
                                repo: created.repo.clone(),
                                description: created.description.clone(),
                                default_branch: created.default_branch.clone(),
                                private: created.private,
                                clone_https: created.clone_https.clone(),
                                clone_ssh: created.clone_ssh.clone(),
                                web_url: created.web_url.clone(),
                            },
                        );
                    }
                    state.remote_repos.create_repo.name.clear();
                    state.remote_repos.create_repo.description.clear();
                    state.remote_repos.create_repo.selected_owner = Some(created.owner.clone());
                    state.remote_repos.create_repo.last_error = None;
                    state.remote_repos.create_repo.last_created = Some(created.clone());
                    refresh_account = state.remote_repos.selected_account.clone();
                }
                self.hud = Some(Hud::new(
                    format!("Created {}/{}", created.owner, created.repo),
                    1800,
                ));
            }
            Err(err) => {
                if let Some(state) = self.background_welcome_state_mut() {
                    state.remote_repos.create_repo.last_error = Some(err.to_string());
                }
                return;
            }
        }

        let account = refresh_account.and_then(|account_id| {
            self.config
                .provider_accounts
                .iter()
                .find(|account| account.id == account_id)
                .cloned()
        });
        if let Some(account) = account {
            self.refresh_remote_repositories(&account);
        }
    }

    fn poll_publish_remote_modal_tasks(&mut self) {
        self.poll_publish_remote_owner_task();
        self.poll_publish_remote_create_task();
    }

    fn poll_publish_remote_owner_task(&mut self) {
        let result = {
            let Some(modal) = self.publish_remote_modal.as_mut() else {
                return;
            };
            let result = modal.owners_task.as_mut().and_then(|task| task.poll());
            if result.is_some() {
                modal.owners_task = None;
            }
            result
        };

        let Some(result) = result else {
            return;
        };

        if let Some(modal) = self.publish_remote_modal.as_mut() {
            match result {
                Ok(owners) => {
                    let selected = modal.selected_owner.clone();
                    modal.owners = owners;
                    let selected_still_exists = selected.as_ref().is_some_and(|login| {
                        modal.owners.iter().any(|owner| owner.login == *login)
                    });
                    if !selected_still_exists {
                        modal.selected_owner =
                            modal.owners.first().map(|owner| owner.login.clone());
                    }
                    modal.last_error = None;
                }
                Err(err) => {
                    modal.owners.clear();
                    modal.last_error = Some(err.to_string());
                }
            }
        }
    }

    fn poll_publish_remote_create_task(&mut self) {
        let (result, repo_path, branch, remote_name, account_slug) = {
            let Some(modal) = self.publish_remote_modal.as_mut() else {
                return;
            };
            let result = modal.create_task.as_mut().and_then(|task| task.poll());
            if result.is_some() {
                modal.create_task = None;
            }
            (
                result,
                modal.repo_path.clone(),
                modal.branch.clone(),
                modal.remote_name.trim().to_string(),
                modal
                    .selected_account
                    .as_ref()
                    .map(|account| account.slug()),
            )
        };

        let Some(result) = result else {
            return;
        };

        match result {
            Ok(created) => {
                let remote_result = (|| -> anyhow::Result<()> {
                    let Some(ws) = self.workspace_by_path_mut(&repo_path) else {
                        anyhow::bail!("repository is no longer open");
                    };
                    let remote_exists = ws
                        .repo
                        .list_remotes()
                        .ok()
                        .unwrap_or_default()
                        .iter()
                        .any(|remote| remote.name == remote_name);
                    if remote_exists {
                        ws.repo
                            .update_remote_urls(&remote_name, &created.clone_https, None)?;
                    } else {
                        ws.repo
                            .add_remote(&remote_name, &created.clone_https, None)?;
                    }
                    Ok(())
                })();

                if let Err(err) = remote_result {
                    if let Some(modal) = self.publish_remote_modal.as_mut() {
                        modal.last_error = Some(format!(
                            "remote repo was created, but adding local remote failed: {err:#}"
                        ));
                    }
                    return;
                }

                let mut settings = self.config.repo_settings_for(&repo_path);
                settings.default_remote = Some(remote_name.clone());
                settings.provider_account = account_slug;
                self.config.set_repo_settings(&repo_path, settings);
                let _ = self.config.save();

                self.publish_remote_modal = None;
                self.start_push_for_repo_path(&repo_path, &remote_name, &branch, false, true);
                self.hud = Some(Hud::new(
                    format!(
                        "Created {}/{} — publishing {branch}",
                        created.owner, created.repo
                    ),
                    2200,
                ));
            }
            Err(err) => {
                if let Some(modal) = self.publish_remote_modal.as_mut() {
                    modal.last_error = Some(err.to_string());
                }
            }
        }
    }

    pub fn open_settings(&mut self) {
        self.open_settings_section(ui::settings::SettingsSection::General);
    }

    pub fn open_settings_section(&mut self, section: ui::settings::SettingsSection) {
        if let Some(modal) = self.settings_modal.as_mut() {
            modal.section = section;
            modal.feedback = None;
            self.settings_open = true;
            return;
        }

        let (repo_path, repo_settings, remotes) = match &self.view {
            View::Workspace(tabs) => {
                let ws = tabs.current();
                (
                    Some(ws.repo.path().to_path_buf()),
                    self.config.repo_settings_for(ws.repo.path()),
                    ws.repo.list_remotes().ok().unwrap_or_default(),
                )
            }
            View::Welcome(_) | View::OpeningRepo(_) => (None, RepoSettings::default(), Vec::new()),
        };

        self.settings_modal = Some(SettingsModal {
            section,
            search_query: String::new(),
            language: self.config.ui_language,
            theme: self.config.theme.clone(),
            repo_path,
            default_remote: repo_settings.default_remote,
            pull_strategy: repo_settings.pull_strategy,
            remotes: remotes
                .into_iter()
                .map(|remote| RemoteDraft {
                    name: remote.name,
                    fetch_url: remote.fetch_url.unwrap_or_default(),
                    push_url: remote.push_url.unwrap_or_default(),
                    rename_to: String::new(),
                })
                .collect(),
            new_remote_name: String::new(),
            new_fetch_url: String::new(),
            new_push_url: String::new(),
            provider_account_slug: repo_settings.provider_account,
            integrations: ui::settings::integrations::IntegrationsDraft::from_config(&self.config),
            ai: ui::settings::ai::AiDraft::from_config(&self.config, &self.secret_store),
            feedback: None,
            identity_name: String::new(),
            identity_email: String::new(),
            identity_global: false,
            identity_loaded: false,
            worktrees: None,
            count_objects: None,
            sparse_checkout: None,
            sparse_patterns_draft: String::new(),
            submodules: None,
            window_size: Some(self.config.settings_window.clone()),
            reset_scope: None,
        });
        self.settings_open = true;
    }

    pub fn default_clone_parent(&self) -> PathBuf {
        let Some(home) = dirs::home_dir() else {
            return PathBuf::from(".");
        };

        let candidates = [
            home.join("Documents").join("dev"),
            home.join("dev"),
            home.join("Documents"),
            home.clone(),
        ];

        candidates
            .into_iter()
            .find(|path| path.exists())
            .unwrap_or_else(|| home.join("dev"))
    }

    pub fn open_rebase_modal_for_head(&mut self) {
        let modal = (|| -> Result<RebaseModalState> {
            let View::Workspace(tabs) = &mut self.view else {
                bail!("no open repository");
            };
            let ws = tabs.current_mut();
            if !matches!(ws.repo.state(), crate::git::RepoState::Clean) {
                bail!("finish or abort the current git operation first");
            }
            let branch = ws
                .repo
                .head_name()
                .context("interactive rebase requires a checked-out branch")?;
            let commits = ws.repo.linear_head_commits(24)?;
            if commits.is_empty() {
                anyhow::bail!("no commits available to rebase");
            }
            let base = commits
                .first()
                .and_then(|c| c.parent)
                .context("root-commit rebases are not yet supported")?;
            let items = commits
                .into_iter()
                .map(|commit| RebasePlanItem {
                    oid: commit.oid,
                    summary: commit.summary,
                    author: commit.author,
                    timestamp: commit.timestamp,
                    action: RebaseAction::Pick,
                    original_message: commit.message.clone(),
                    edited_message: commit.message,
                })
                .collect();

            Ok(RebaseModalState {
                branch,
                base,
                backup_current_state: true,
                items,
                selected_idx: 0,
                last_error: None,
            })
        })();

        match modal {
            Ok(modal) => {
                if let View::Workspace(tabs) = &mut self.view {
                    tabs.current_mut().rebase_modal = Some(modal);
                }
            }
            Err(e) => {
                self.last_error = Some(format!("interactive rebase: {e:#}"));
            }
        }
    }

    pub fn start_rebase_session(&mut self) {
        let (scope, stashed) = {
            let View::Workspace(tabs) = &mut self.view else {
                return;
            };
            let ws = tabs.current_mut();
            let Some(mut modal) = ws.rebase_modal.take() else {
                return;
            };
            let scope = ws.graph_scope;
            let setup = (|| -> Result<bool> {
                let steps = build_rebase_steps(&modal.items)?;
                let stashed = ws.repo.auto_stash_if_dirty("interactive rebase")?;
                let backup_ref = if modal.backup_current_state {
                    Some(ws.repo.create_backup_branch(&modal.branch)?)
                } else {
                    None
                };
                let before_snapshot = journal::capture(ws.repo.path()).ok();
                ws.repo.reset(crate::actions::ResetMode::Hard, modal.base)?;
                ws.rebase_session = Some(RebaseSession {
                    branch: modal.branch.clone(),
                    base: modal.base,
                    backup_ref,
                    steps,
                    next_index: 0,
                    before_snapshot,
                });
                Ok(stashed)
            })();

            match setup {
                Ok(stashed) => (scope, stashed),
                Err(e) => {
                    modal.last_error = Some(format!("{e:#}"));
                    ws.rebase_modal = Some(modal);
                    return;
                }
            }
        };

        if stashed {
            self.hud = Some(Hud::new("Stashed dirty changes before rebase", 1800));
        }
        self.advance_rebase_session();
        self.rebuild_graph(scope);
    }

    pub fn advance_rebase_session(&mut self) {
        enum Advance {
            Blocked {
                msg: String,
                scope: GraphScope,
            },
            Failed(String),
            Finished {
                branch: String,
                base: gix::ObjectId,
                before: Option<RepoSnapshot>,
                after: Option<RepoSnapshot>,
                scope: GraphScope,
            },
            Noop,
        }

        let outcome = {
            let View::Workspace(tabs) = &mut self.view else {
                return;
            };
            let ws = tabs.current_mut();
            let Some(session) = ws.rebase_session.as_mut() else {
                return;
            };
            let scope = ws.graph_scope;

            loop {
                if session.next_index >= session.steps.len() {
                    let branch = session.branch.clone();
                    let base = session.base;
                    let before = session.before_snapshot.clone();
                    let after = journal::capture(ws.repo.path()).ok();
                    ws.rebase_session = None;
                    break Advance::Finished {
                        branch,
                        base,
                        before,
                        after,
                        scope,
                    };
                }

                let step = session.steps[session.next_index].clone();
                match step.action {
                    RebaseAction::Drop => {
                        session.next_index += 1;
                    }
                    RebaseAction::Pick | RebaseAction::Reword => {
                        match ws.repo.start_cherry_pick_apply(step.oid) {
                            Ok(true) => {
                                break Advance::Blocked {
                                    msg: format!(
                                        "Resolve conflicts for {} to continue rebase",
                                        short_sha(&step.oid)
                                    ),
                                    scope,
                                };
                            }
                            Ok(false) => {
                                let message = if matches!(step.action, RebaseAction::Reword) {
                                    Some(step.message.as_str())
                                } else {
                                    None
                                };
                                if let Err(e) =
                                    ws.repo.finish_pending_pick_commit(step.oid, message)
                                {
                                    break Advance::Failed(format!("rebase step: {e:#}"));
                                }
                                session.next_index += 1;
                            }
                            Err(e) => break Advance::Failed(format!("rebase step: {e:#}")),
                        }
                    }
                    RebaseAction::Squash | RebaseAction::Fixup => match ws.repo.start_cherry_pick_apply(step.oid) {
                        Ok(true) => {
                            break Advance::Blocked {
                                msg: format!(
                                    "Resolve conflicts for {} to continue rebase",
                                    short_sha(&step.oid)
                                ),
                                scope,
                            };
                        }
                        Ok(false) => {
                            // Both Squash and Fixup finish via the
                            // same "combine into previous commit"
                            // path — Fixup already baked the discarded-
                            // message decision into `step.message`
                            // during plan compilation (it carries the
                            // previous message verbatim).
                            if let Err(e) = ws.repo.finish_pending_pick_squash(&step.message) {
                                break Advance::Failed(format!(
                                    "{} step: {e:#}",
                                    if matches!(step.action, RebaseAction::Fixup) {
                                        "fixup"
                                    } else {
                                        "squash"
                                    }
                                ));
                            }
                            session.next_index += 1;
                        }
                        Err(e) => break Advance::Failed(format!("squash step: {e:#}")),
                    },
                }
            }
        };

        match outcome {
            Advance::Blocked { msg, scope } => {
                self.hud = Some(Hud::new(msg, 2200));
                self.rebuild_graph(scope);
            }
            Advance::Failed(err) => {
                self.last_error = Some(err);
            }
            Advance::Finished {
                branch,
                base,
                before,
                after,
                scope,
            } => {
                if let (Some(before), Some(after)) = (before, after) {
                    self.journal_record(
                        Operation::Rebase {
                            branch: branch.clone(),
                            onto: short_sha(&base),
                        },
                        before,
                        after,
                    );
                }
                self.hud = Some(Hud::new(
                    format!("Rebased {branch} onto {}", short_sha(&base)),
                    2200,
                ));
                self.rebuild_graph(scope);
            }
            Advance::Noop => {}
        }
    }

    pub fn continue_rebase_session(&mut self) {
        let step = {
            let View::Workspace(tabs) = &mut self.view else {
                return;
            };
            let ws = tabs.current_mut();
            let Some(session) = ws.rebase_session.as_ref() else {
                return;
            };
            session.steps.get(session.next_index).cloned()
        };

        let Some(step) = step else {
            self.advance_rebase_session();
            return;
        };

        let result = {
            let View::Workspace(tabs) = &mut self.view else {
                return;
            };
            let ws = tabs.current_mut();
            if ws.repo.pending_operation_has_conflicts().unwrap_or(false) {
                Err(anyhow::anyhow!("conflicts remain unresolved"))
            } else {
                match step.action {
                    RebaseAction::Pick => ws.repo.finish_pending_pick_commit(step.oid, None),
                    RebaseAction::Reword => ws
                        .repo
                        .finish_pending_pick_commit(step.oid, Some(step.message.as_str())),
                    RebaseAction::Squash | RebaseAction::Fixup => {
                        ws.repo.finish_pending_pick_squash(&step.message)
                    }
                    RebaseAction::Drop => Ok(step.oid),
                }
            }
        };

        match result {
            Ok(_) => {
                if let View::Workspace(tabs) = &mut self.view {
                    if let Some(session) = tabs.current_mut().rebase_session.as_mut() {
                        session.next_index += 1;
                    }
                }
                self.advance_rebase_session();
            }
            Err(e) => {
                self.last_error = Some(format!("continue rebase: {e:#}"));
            }
        }
    }

    pub fn abort_rebase_session(&mut self) {
        let outcome = (|| -> Result<(GraphScope, String)> {
            let View::Workspace(tabs) = &mut self.view else {
                bail!("no open repository");
            };
            let ws = tabs.current_mut();
            let Some(session) = ws.rebase_session.take() else {
                bail!("no rebase session in progress");
            };
            if !matches!(ws.repo.state(), crate::git::RepoState::Clean) {
                ws.repo.abort_operation().ok();
            }

            let restore_target = if let Some(backup) = session.backup_ref.as_ref() {
                ws.repo.tip_of(backup, false)?
            } else {
                let head = session
                    .before_snapshot
                    .as_ref()
                    .and_then(|s| (!s.head.is_empty()).then_some(s.head.as_str()))
                    .context("no backup ref or snapshot available to abort rebase")?;
                gix::ObjectId::from_hex(head.as_bytes())
                    .map_err(|e| anyhow::anyhow!("parse head oid: {e}"))?
            };
            ws.repo
                .reset(crate::actions::ResetMode::Hard, restore_target)?;
            Ok((ws.graph_scope, session.branch))
        })();

        match outcome {
            Ok((scope, branch)) => {
                self.hud = Some(Hud::new(format!("Aborted rebase on {branch}"), 1800));
                self.rebuild_graph(scope);
            }
            Err(e) => {
                self.last_error = Some(format!("abort rebase: {e:#}"));
            }
        }
    }

    pub fn continue_conflict_operation(&mut self) {
        let is_rebase = matches!(
            &self.view,
            View::Workspace(tabs) if tabs.current().rebase_session.is_some()
        );
        if is_rebase {
            self.continue_rebase_session();
            return;
        }

        let outcome = (|| -> Result<(String, GraphScope)> {
            let View::Workspace(tabs) = &mut self.view else {
                bail!("no open repository");
            };
            let ws = tabs.current_mut();
            if ws.repo.pending_operation_has_conflicts()? {
                bail!("conflicts remain unresolved");
            }

            let msg = match ws.repo.state() {
                crate::git::RepoState::Merge => {
                    let oid = ws.repo.continue_merge()?;
                    format!("Created merge commit {}", short_sha(&oid))
                }
                crate::git::RepoState::CherryPick | crate::git::RepoState::CherryPickSequence => {
                    let oid = ws.repo.continue_cherry_pick()?;
                    format!("Cherry-pick continued as {}", short_sha(&oid))
                }
                crate::git::RepoState::Revert | crate::git::RepoState::RevertSequence => {
                    let oid = ws.repo.continue_revert()?;
                    format!("Revert continued as {}", short_sha(&oid))
                }
                other => bail!("cannot continue repository state {other:?}"),
            };
            Ok((msg, ws.graph_scope))
        })();

        match outcome {
            Ok((msg, scope)) => {
                self.hud = Some(Hud::new(msg, 2000));
                self.rebuild_graph(scope);
            }
            Err(e) => {
                self.last_error = Some(format!("continue operation: {e:#}"));
            }
        }
    }

    pub fn abort_conflict_operation(&mut self) {
        let is_rebase = matches!(
            &self.view,
            View::Workspace(tabs) if tabs.current().rebase_session.is_some()
        );
        if is_rebase {
            self.abort_rebase_session();
            return;
        }

        let outcome = (|| -> Result<GraphScope> {
            let View::Workspace(tabs) = &mut self.view else {
                bail!("no open repository");
            };
            let ws = tabs.current_mut();
            ws.repo.abort_operation()?;
            Ok(ws.graph_scope)
        })();

        match outcome {
            Ok(scope) => {
                self.hud = Some(Hud::new("Aborted in-progress operation", 1800));
                self.rebuild_graph(scope);
            }
            Err(e) => {
                self.last_error = Some(format!("abort operation: {e:#}"));
            }
        }
    }

    fn release_inactive_tab_caches(&mut self) {
        let View::Workspace(tabs) = &mut self.view else {
            return;
        };
        // Only drop the image cache for inactive tabs. Image blobs can be
        // multi-MB each and back GPU textures, so they're the worst offender
        // for idle memory. Keep `graph_view` and `current_diff` around —
        // they're cheap relative to the cost of rebuilding them on the UI
        // thread every time the user clicks a tab, which is what caused the
        // visible freeze on large repos (graph rebuild + gix
        // `find_similar` rerun for the selected commit's diff).
        for (idx, ws) in tabs.tabs.iter_mut().enumerate() {
            if tabs.launcher_active || idx != tabs.active {
                ws.set_image_cache(None);
                // Snapshot cache holds the full decoded blob text — drop
                // it for inactive tabs so we don't hold a copy of every
                // file the user has viewed across N tabs.
                ws.snapshot_cache = None;
            }
        }
    }

    fn restore_active_tab_cache(&mut self) {
        self.restore_active_graph_cache();
        self.restore_active_diff_cache();
    }

    fn restore_active_graph_cache(&mut self) {
        let View::Workspace(tabs) = &mut self.view else {
            return;
        };
        if tabs.launcher_active {
            return;
        }
        let ws = tabs.current_mut();
        if ws.graph_view.is_some() {
            return;
        }
        // Don't queue a second rebuild for the same scope — poll_graph_tasks
        // will install the in-flight result when it lands.
        if ws
            .graph_task
            .as_ref()
            .is_some_and(|task| task.scope == ws.graph_scope)
        {
            return;
        }
        let scope = ws.graph_scope;
        spawn_graph_task(ws, scope, &self.egui_ctx);
    }

    fn restore_active_diff_cache(&mut self) {
        let View::Workspace(tabs) = &mut self.view else {
            return;
        };
        if tabs.launcher_active {
            return;
        }
        let ws = tabs.current_mut();
        let Some(oid) = ws.selected_commit else {
            return;
        };
        if ws.current_diff.is_some() {
            return;
        }
        // Already computing this exact diff — don't queue a duplicate.
        if ws.diff_task.as_ref().is_some_and(|task| task.oid == oid) {
            return;
        }
        ws.set_image_cache(None);

        // Spawn a worker. `diff_for_commit` runs git's `find_similar`
        // which is O(files²) in the worst case; doing it on the UI thread
        // — which is what this function used to do — froze the window for
        // several seconds on every tab switch when the selected commit was
        // a large merge. `poll_diff_tasks` will install the result on a
        // future frame via the usual async diff path.
        let (tx, rx) = std::sync::mpsc::channel();
        let repo_path = ws.repo.path().to_path_buf();
        let ctx_clone = self.egui_ctx.clone();
        std::thread::spawn(move || {
            let result = crate::git::diff_for_commit(&repo_path, oid).map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
            ctx_clone.request_repaint();
        });
        ws.diff_task = Some(DiffTask {
            oid,
            started_at: std::time::Instant::now(),
            rx,
        });
    }

    pub fn ensure_active_forge_loaded(&mut self) {
        if self.forge_refresh_task.is_some()
            || self.forge_create_pr_task.is_some()
            || self.forge_create_issue_task.is_some()
        {
            return;
        }
        let target = {
            let View::Workspace(tabs) = &mut self.view else {
                return;
            };
            if tabs.launcher_active {
                return;
            }
            let ws = tabs.current_mut();
            if ws.forge.loading {
                return;
            }
            let Some(context) = crate::forge::resolve_repo(&self.config, &ws.repo) else {
                reset_forge_state(&mut ws.forge);
                return;
            };
            if let Some(current) = &ws.forge.repo {
                if current.owner == context.owner
                    && current.repo == context.repo
                    && ws.forge.loaded_once
                {
                    return;
                }
            }
            ws.forge.repo = Some(context.clone());
            ws.forge.loading = true;
            ws.forge.last_error = None;
            Some((ws.repo.path().to_path_buf(), context))
        };
        let Some((repo_path, context)) = target else {
            return;
        };

        let context_for_task = context.clone();
        self.forge_refresh_task = Some(providers::runtime::ProviderTask::spawn(async move {
            let provider = providers::build(&context_for_task.kind).await;
            let client = providers::default_http_client();
            let token = providers::pat::load_pat(&context_for_task.account_id)
                .map_err(|err| providers::ProviderError::Network(err.to_string()))?
                .ok_or(providers::ProviderError::Unauthorized)?;

            let repo_meta = provider
                .discover_repo(
                    &client,
                    Some(&token),
                    &context_for_task.owner,
                    &context_for_task.repo,
                )
                .await?;
            let pull_requests = provider
                .list_pull_requests(
                    &client,
                    &token,
                    &context_for_task.owner,
                    &context_for_task.repo,
                    providers::PrState::Open,
                )
                .await?;
            let issues = provider
                .list_issues(
                    &client,
                    &token,
                    &context_for_task.owner,
                    &context_for_task.repo,
                    providers::IssueState::Open,
                )
                .await?;

            let mut refreshed = context_for_task.clone();
            refreshed.default_branch = repo_meta.default_branch;
            refreshed.private = repo_meta.private;

            if refreshed.pr_template.as_deref().is_none_or(str::is_empty) {
                for candidate in crate::forge::candidate_pr_template_paths() {
                    if let Some(text) = provider
                        .load_repo_text_file(
                            &client,
                            &token,
                            &refreshed.owner,
                            &refreshed.repo,
                            candidate,
                        )
                        .await?
                    {
                        if !text.trim().is_empty() {
                            refreshed.pr_template = Some(text);
                            break;
                        }
                    }
                }
            }
            if refreshed
                .issue_template
                .as_deref()
                .is_none_or(str::is_empty)
            {
                for candidate in crate::forge::candidate_issue_template_paths() {
                    if let Some(text) = provider
                        .load_repo_text_file(
                            &client,
                            &token,
                            &refreshed.owner,
                            &refreshed.repo,
                            candidate,
                        )
                        .await?
                    {
                        if !text.trim().is_empty() {
                            refreshed.issue_template = Some(text);
                            break;
                        }
                    }
                }
            }

            Ok(ForgeRefreshResult {
                repo_path,
                repo: refreshed,
                pull_requests,
                issues,
            })
        }));
    }

    pub fn refresh_active_forge(&mut self) {
        let repo_path = match &self.view {
            View::Workspace(tabs) if !tabs.launcher_active => {
                Some(tabs.current().repo.path().to_path_buf())
            }
            _ => None,
        };
        if let Some(repo_path) = repo_path {
            self.refresh_forge_for_repo_path(&repo_path);
        }
    }

    pub fn open_pull_request_modal(&mut self) {
        let View::Workspace(tabs) = &mut self.view else {
            return;
        };
        if tabs.launcher_active {
            return;
        }
        let ws = tabs.current_mut();
        let head_branch = ws.repo.head_name();
        let (head_ready, head_hint) = pull_request_head_status(&ws.repo, head_branch.as_deref());
        crate::forge::open_pull_request_modal(&mut ws.forge, head_branch, head_ready, head_hint);
    }

    pub fn open_issue_modal(&mut self) {
        let View::Workspace(tabs) = &mut self.view else {
            return;
        };
        if tabs.launcher_active {
            return;
        }
        let ws = tabs.current_mut();
        crate::forge::open_issue_modal(&mut ws.forge);
    }

    pub fn submit_pull_request(&mut self) {
        if self.forge_create_pr_task.is_some() {
            return;
        }
        let target = {
            let View::Workspace(tabs) = &mut self.view else {
                return;
            };
            if tabs.launcher_active {
                return;
            }
            let ws = tabs.current_mut();
            let Some(context) = ws.forge.repo.clone() else {
                ws.forge.last_error = Some("Connect a supported provider first.".into());
                return;
            };
            let Some(draft) = ws.forge.pr_modal.clone() else {
                return;
            };
            if draft.title.trim().is_empty() {
                if let Some(modal) = ws.forge.pr_modal.as_mut() {
                    modal.last_error = Some("Enter a pull request title.".into());
                }
                return;
            }
            if !draft.head_ready {
                if let Some(modal) = ws.forge.pr_modal.as_mut() {
                    modal.last_error = Some(draft.head_hint.clone().unwrap_or_else(|| {
                        "Push this branch before creating a pull request.".into()
                    }));
                }
                return;
            }
            if draft.base.trim().is_empty() {
                if let Some(modal) = ws.forge.pr_modal.as_mut() {
                    modal.last_error = Some("Choose a base branch first.".into());
                }
                return;
            }
            Some((ws.repo.path().to_path_buf(), context, draft))
        };
        let Some((repo_path, context, draft)) = target else {
            return;
        };
        self.forge_create_pr_task = Some(providers::runtime::ProviderTask::spawn(async move {
            let provider = providers::build(&context.kind).await;
            let client = providers::default_http_client();
            let token = providers::pat::load_pat(&context.account_id)
                .map_err(|err| providers::ProviderError::Network(err.to_string()))?
                .ok_or(providers::ProviderError::Unauthorized)?;
            let pull_request = provider
                .create_pull_request(
                    &client,
                    &token,
                    &providers::PullRequestDraft {
                        owner: context.owner.clone(),
                        repo: context.repo.clone(),
                        title: draft.title,
                        body: draft.body,
                        head: draft.head,
                        base: draft.base,
                        draft: draft.draft,
                    },
                )
                .await?;
            Ok(ForgeCreatePrResult {
                repo_path,
                pull_request,
            })
        }));
    }

    pub fn submit_issue(&mut self) {
        if self.forge_create_issue_task.is_some() {
            return;
        }
        let target = {
            let View::Workspace(tabs) = &mut self.view else {
                return;
            };
            if tabs.launcher_active {
                return;
            }
            let ws = tabs.current_mut();
            let Some(context) = ws.forge.repo.clone() else {
                ws.forge.last_error = Some("Connect a supported provider first.".into());
                return;
            };
            let Some(draft) = ws.forge.issue_modal.clone() else {
                return;
            };
            if draft.title.trim().is_empty() {
                if let Some(modal) = ws.forge.issue_modal.as_mut() {
                    modal.last_error = Some("Enter an issue title.".into());
                }
                return;
            }
            Some((ws.repo.path().to_path_buf(), context, draft))
        };
        let Some((repo_path, context, draft)) = target else {
            return;
        };
        self.forge_create_issue_task = Some(providers::runtime::ProviderTask::spawn(async move {
            let provider = providers::build(&context.kind).await;
            let client = providers::default_http_client();
            let token = providers::pat::load_pat(&context.account_id)
                .map_err(|err| providers::ProviderError::Network(err.to_string()))?
                .ok_or(providers::ProviderError::Unauthorized)?;
            let issue = provider
                .create_issue(
                    &client,
                    &token,
                    &providers::IssueDraft {
                        owner: context.owner.clone(),
                        repo: context.repo.clone(),
                        title: draft.title,
                        body: draft.body,
                    },
                )
                .await?;
            Ok(ForgeCreateIssueResult { repo_path, issue })
        }));
    }

    fn poll_forge_tasks(&mut self) {
        self.poll_forge_refresh_task();
        self.poll_forge_create_pr_task();
        self.poll_forge_create_issue_task();
    }

    fn poll_forge_refresh_task(&mut self) {
        let Some(task) = self.forge_refresh_task.as_mut() else {
            return;
        };
        let Some(result) = task.poll() else {
            return;
        };
        self.forge_refresh_task = None;

        match result {
            Ok(refresh) => {
                if let Some(ws) = self.workspace_by_path_mut(&refresh.repo_path) {
                    crate::forge::merge_refresh(&mut ws.forge, refresh);
                }
            }
            Err(err) => {
                if let View::Workspace(tabs) = &mut self.view {
                    if !tabs.launcher_active {
                        let ws = tabs.current_mut();
                        ws.forge.loading = false;
                        ws.forge.last_error = Some(err.to_string());
                    }
                }
            }
        }
    }

    fn poll_forge_create_pr_task(&mut self) {
        let Some(task) = self.forge_create_pr_task.as_mut() else {
            return;
        };
        let Some(result) = task.poll() else {
            return;
        };
        self.forge_create_pr_task = None;

        match result {
            Ok(created) => {
                if let Some(ws) = self.workspace_by_path_mut(&created.repo_path) {
                    ws.forge.pr_modal = None;
                    ws.forge.selected = Some(crate::forge::ForgeSelection::PullRequest(
                        created.pull_request.number,
                    ));
                }
                self.hud = Some(Hud::new(
                    format!("Created PR #{}", created.pull_request.number),
                    1800,
                ));
                self.refresh_forge_for_repo_path(&created.repo_path);
            }
            Err(err) => {
                if let View::Workspace(tabs) = &mut self.view {
                    if !tabs.launcher_active {
                        let ws = tabs.current_mut();
                        if let Some(modal) = ws.forge.pr_modal.as_mut() {
                            modal.last_error = Some(err.to_string());
                        }
                    }
                }
            }
        }
    }

    fn poll_forge_create_issue_task(&mut self) {
        let Some(task) = self.forge_create_issue_task.as_mut() else {
            return;
        };
        let Some(result) = task.poll() else {
            return;
        };
        self.forge_create_issue_task = None;

        match result {
            Ok(created) => {
                if let Some(ws) = self.workspace_by_path_mut(&created.repo_path) {
                    ws.forge.issue_modal = None;
                    ws.forge.selected =
                        Some(crate::forge::ForgeSelection::Issue(created.issue.number));
                }
                self.hud = Some(Hud::new(
                    format!("Created issue #{}", created.issue.number),
                    1800,
                ));
                self.refresh_forge_for_repo_path(&created.repo_path);
            }
            Err(err) => {
                if let View::Workspace(tabs) = &mut self.view {
                    if !tabs.launcher_active {
                        let ws = tabs.current_mut();
                        if let Some(modal) = ws.forge.issue_modal.as_mut() {
                            modal.last_error = Some(err.to_string());
                        }
                    }
                }
            }
        }
    }

    pub fn workspace_by_path_mut(&mut self, repo_path: &Path) -> Option<&mut WorkspaceState> {
        let View::Workspace(tabs) = &mut self.view else {
            return None;
        };
        tabs.tabs.iter_mut().find(|ws| ws.repo.path() == repo_path)
    }

    pub fn workspace_by_path(&self, repo_path: &Path) -> Option<&WorkspaceState> {
        let View::Workspace(tabs) = &self.view else {
            return None;
        };
        tabs.tabs.iter().find(|ws| ws.repo.path() == repo_path)
    }

    fn refresh_forge_for_repo_path(&mut self, repo_path: &Path) {
        if let Some(ws) = self.workspace_by_path_mut(repo_path) {
            reset_forge_state(&mut ws.forge);
        }
        if matches!(&self.view, View::Workspace(tabs) if !tabs.launcher_active && tabs.current().repo.path() == repo_path)
        {
            self.ensure_active_forge_loaded();
        }
    }

    // ------------ journal helpers ------------

    pub fn journal_record(&mut self, op: Operation, before: RepoSnapshot, after: RepoSnapshot) {
        self.journal_record_with_source(op, before, after, OpSource::Ui);
    }

    pub fn journal_record_with_source(
        &mut self,
        op: Operation,
        before: RepoSnapshot,
        after: RepoSnapshot,
        source: OpSource,
    ) {
        let View::Workspace(tabs) = &mut self.view else {
            return;
        };
        let ws = tabs.current_mut();
        let Some(journal) = ws.journal.as_mut() else {
            return;
        };
        if let Err(e) = journal.record(op, before, after, source) {
            self.last_error = Some(format!("journal record: {e:#}"));
        }
    }

    pub fn journal_record_mcp(
        &mut self,
        agent: impl Into<String>,
        op: Operation,
        before: RepoSnapshot,
        after: RepoSnapshot,
    ) {
        self.journal_record_with_source(
            op,
            before,
            after,
            OpSource::Mcp {
                agent: agent.into(),
            },
        );
    }

    pub fn undo(&mut self) {
        if !self.nav_debounce_ok() {
            return;
        }
        if let Err(e) = self.spawn_nav(NavRequest::Undo) {
            self.last_error = Some(format!("undo failed: {e:#}"));
        }
    }

    pub fn redo(&mut self) {
        if !self.nav_debounce_ok() {
            return;
        }
        if let Err(e) = self.spawn_nav(NavRequest::Redo) {
            self.last_error = Some(format!("redo failed: {e:#}"));
        }
    }

    /// Common entry point for "kick off async navigation".
    ///
    /// Reads the journal cursor / target snapshot synchronously (cheap),
    /// then hands off to a background thread so the slow part — auto-stash
    /// of dirty tracked files + force-checkout of the target HEAD — never
    /// blocks the egui update loop. Returns immediately. The completion
    /// handler in `poll_nav_tasks` advances the cursor and rebuilds the
    /// graph once the worker reports success.
    fn spawn_nav(&mut self, req: NavRequest) -> Result<()> {
        let View::Workspace(tabs) = &mut self.view else {
            return Ok(());
        };
        let ws = tabs.current_mut();

        // Coalesce: if a nav is already in flight, ignore the new request.
        // We deliberately don't queue — queueing would mean a second
        // Cmd+Z press during a slow undo also runs after, which usually
        // isn't what the user wants (they pressed it to "speed things up"
        // not to chain operations they couldn't see the result of).
        if ws.nav_task.is_some() {
            self.hud = Some(Hud::new("still navigating…", 1200));
            return Ok(());
        }

        let Some(journal) = ws.journal.as_mut() else {
            return Ok(());
        };

        // Resolve the request to (target snapshot, kind, label, reason).
        let (target, kind, label, reason) = match req {
            NavRequest::Undo => {
                let Some(entry) = journal.peek_undo().cloned() else {
                    self.hud = Some(Hud::new("nothing to undo", 1200));
                    return Ok(());
                };
                let label = format!("Undo: {}", entry.operation.label());
                (
                    entry.before,
                    journal::JournalNavKind::Undo,
                    label,
                    "undo".to_string(),
                )
            }
            NavRequest::Redo => {
                let Some(entry) = journal.peek_redo().cloned() else {
                    self.hud = Some(Hud::new("nothing to redo", 1200));
                    return Ok(());
                };
                let label = format!("Redo: {}", entry.operation.label());
                (
                    entry.after,
                    journal::JournalNavKind::Redo,
                    label,
                    "redo".to_string(),
                )
            }
            NavRequest::RestoreToBefore { entry_id } => {
                let Some(idx) = journal.entries.iter().position(|e| e.id == entry_id) else {
                    self.hud = Some(Hud::new("entry not found", 1200));
                    return Ok(());
                };
                let entry = journal.entries[idx].clone();
                let label = format!("Restored before: {}", entry.operation.label());
                (
                    entry.before,
                    journal::JournalNavKind::RestoreToBefore { entry_id },
                    label,
                    "restore".to_string(),
                )
            }
        };

        let repo_path = ws.repo.path().to_path_buf();
        ws.nav_task = Some(crate::journal::JournalNavTask::spawn(
            repo_path, target, reason, kind, label,
        ));
        // Drive a repaint so the spinner shows up next frame even if the
        // user isn't moving the mouse.
        Ok(())
    }

    fn nav_debounce_ok(&mut self) -> bool {
        let now = Instant::now();
        // Debounce: ignore key repeats closer than NAV_DEBOUNCE.
        if let Some(&last) = self.nav_history.back() {
            if now.duration_since(last) < NAV_DEBOUNCE {
                return false;
            }
        }
        self.nav_history.push_back(now);
        // Retain only within PANIC_WINDOW.
        while let Some(&front) = self.nav_history.front() {
            if now.duration_since(front) > PANIC_WINDOW {
                self.nav_history.pop_front();
            } else {
                break;
            }
        }
        true
    }

    pub fn panic_detector_active(&self) -> bool {
        self.nav_history.len() >= PANIC_THRESHOLD
    }

    pub fn open_panic_recovery(&mut self) {
        self.panic_modal_open = true;
    }

    /// Kick off a blame for whichever file is currently selected in
    /// the diff view. If nothing is selected (welcome screen, no diff
    /// loaded, or the selected file is a binary/image without a path
    /// we can blame) report via the notification center rather than
    /// opening an empty modal.
    /// Save the active workspace's journal as JSON. Opens an
    /// `rfd::FileDialog` for the destination; success/failure reports
    /// through the notification center.
    ///
    /// We render the JSON up front (while the workspace borrow is
    /// live) so the blocking file dialog doesn't hold an `&Journal`
    /// reference across the await-equivalent. `Journal` isn't
    /// `Clone`-able by design (cursor file + dir handle would be
    /// nonsensical to duplicate) — the extracted JSON text is all we
    /// need.
    pub fn export_journal_to_file(&mut self) {
        let (repo_display, json_text) = match &self.view {
            View::Workspace(tabs) => {
                let ws = tabs.current();
                let Some(journal) = ws.journal.as_ref() else {
                    self.notify_warn(
                        "No journal for this repository yet — make a change first.",
                    );
                    return;
                };
                let text = match journal.export_json() {
                    Ok(t) => t,
                    Err(err) => {
                        self.notify_err_with_detail(
                            "Journal export failed",
                            format!("{err:#}"),
                        );
                        return;
                    }
                };
                let name = ws
                    .repo
                    .path()
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "repo".to_string());
                (name, text)
            }
            _ => {
                self.notify_warn("Open a repository first.");
                return;
            }
        };
        let suggested = format!("mergefox-journal-{repo_display}.json");
        let Some(dest) = rfd::FileDialog::new()
            .set_file_name(&suggested)
            .add_filter("JSON", &["json"])
            .save_file()
        else {
            return; // user cancelled
        };
        match std::fs::write(&dest, json_text) {
            Ok(()) => self.notify_ok(format!(
                "Exported journal → {}",
                dest.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| dest.display().to_string())
            )),
            Err(err) => self.notify_err_with_detail("Journal export failed", format!("{err:#}")),
        }
    }

    pub fn start_blame_for_selected_file(&mut self) {
        let (repo_path, file_path) = match &self.view {
            View::Workspace(tabs) => {
                let ws = tabs.current();
                let file = ws
                    .current_diff
                    .as_ref()
                    .and_then(|d| {
                        ws.selected_file_idx.and_then(|idx| d.files.get(idx))
                    })
                    .map(|f| f.new_path.clone().or_else(|| f.old_path.clone()))
                    .flatten();
                match file {
                    Some(p) => (ws.repo.path().to_path_buf(), p),
                    None => {
                        self.notify_warn(
                            "Select a file in the diff view before running blame.",
                        );
                        return;
                    }
                }
            }
            _ => {
                self.notify_warn("Open a repository first.");
                return;
            }
        };
        self.blame.open = true;
        self.blame.error = None;
        self.blame.result = None;
        self.blame.task = Some(crate::ui::blame::BlameTask::spawn(
            repo_path,
            file_path.into(),
        ));
    }

    /// Drain any completed navigation tasks across all open tabs.
    ///
    /// On success the cursor is finally advanced (we deferred this until
    /// after the worker confirmed the git work landed — that way a failed
    /// or aborted nav leaves the journal cursor untouched and the user can
    /// retry without state divergence). On error we surface the message
    /// and leave the cursor where it was.
    fn poll_nav_tasks(&mut self) {
        let View::Workspace(tabs) = &mut self.view else {
            return;
        };
        // Walk all tabs so a nav that finishes while the user is on a
        // different tab is still completed (otherwise the receiver would
        // sit unread until the user comes back).
        let mut completions: Vec<(
            usize,
            std::result::Result<(), String>,
            journal::JournalNavKind,
            String,
        )> = Vec::new();
        for (idx, tab) in tabs.tabs.iter_mut().enumerate() {
            if let Some(task) = &tab.nav_task {
                if let Some(result) = task.poll() {
                    let kind = task.kind;
                    let label = task.label.clone();
                    tab.nav_task = None;
                    completions.push((idx, result, kind, label));
                }
            }
        }
        for (tab_idx, result, kind, label) in completions {
            self.finalize_nav(tab_idx, result, kind, label);
        }
    }

    fn finalize_nav(
        &mut self,
        tab_idx: usize,
        result: std::result::Result<(), String>,
        kind: journal::JournalNavKind,
        label: String,
    ) {
        let scope_for_rebuild = {
            let View::Workspace(tabs) = &mut self.view else {
                return;
            };
            let Some(tab) = tabs.tabs.get_mut(tab_idx) else {
                return;
            };

            match result {
                Err(err) => {
                    self.last_error = Some(format!("nav failed: {err}"));
                    return;
                }
                Ok(()) => {}
            }

            // Worker succeeded — now and only now do we move the cursor.
            if let Some(journal) = tab.journal.as_mut() {
                match kind {
                    journal::JournalNavKind::Undo => {
                        journal.step_back();
                    }
                    journal::JournalNavKind::Redo => {
                        journal.step_forward();
                    }
                    journal::JournalNavKind::RestoreToBefore { entry_id } => {
                        if let Some(idx) = journal.entries.iter().position(|e| e.id == entry_id) {
                            journal.cursor = if idx == 0 { None } else { Some(idx - 1) };
                        }
                    }
                }
            }
            tab.graph_scope
        };

        self.hud = Some(Hud::new(label, 1500));
        self.rebuild_graph(scope_for_rebuild);
    }

    // `handle_nav_result` was retired together with `undo_inner` /
    // `redo_inner`. The new async path advances the cursor and rebuilds
    // the graph in `finalize_nav` once the worker reports success.

    // ------------ background git jobs ------------

    /// Kick off a fetch for the given remote. Returns immediately; UI
    /// polls `active_job` each frame for progress / completion.
    pub fn start_fetch(&mut self, remote: &str) {
        let credentials = self.resolve_https_credentials(remote);
        self.start_job(GitJobKind::Fetch {
            remote: remote.to_string(),
            credentials,
        });
    }

    pub fn start_push(&mut self, remote: &str, branch: &str, force: bool) {
        self.start_push_with_options(remote, branch, force, false);
    }

    pub fn start_push_for_repo_path(
        &mut self,
        repo_path: &Path,
        remote: &str,
        branch: &str,
        force: bool,
        set_upstream: bool,
    ) {
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        let credentials = self.resolve_https_credentials_for_repo_path(repo_path, remote);
        self.start_job_for_repo_path(
            repo_path,
            GitJobKind::Push {
                remote: remote.to_string(),
                refspec,
                force,
                set_upstream,
                credentials,
            },
        );
    }

    fn start_push_with_options(
        &mut self,
        remote: &str,
        branch: &str,
        force: bool,
        set_upstream: bool,
    ) {
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        let credentials = self.resolve_https_credentials(remote);
        self.start_job(GitJobKind::Push {
            remote: remote.to_string(),
            refspec,
            force,
            set_upstream,
            credentials,
        });
    }

    /// Push a single tag (by short name — we'll prefix `refs/tags/`
    /// inside the job). Caller decides the remote; the typical entry
    /// point is the graph's tag-chip context menu which already knows
    /// which remote to target.
    pub fn start_push_tag(&mut self, remote: &str, tag: &str) {
        let credentials = self.resolve_https_credentials(remote);
        self.start_job(GitJobKind::PushTag {
            remote: remote.to_string(),
            tags: vec![tag.to_string()],
            all: false,
            credentials,
        });
    }

    /// `git push <remote> --tags` — every local tag not yet on the
    /// remote. Behind a confirmation prompt because it can dump
    /// hundreds of tags on a repo that's been using mergeFox through
    /// a rebase / tag-creation session.
    pub fn start_push_all_tags(&mut self, remote: &str) {
        let credentials = self.resolve_https_credentials(remote);
        self.start_job(GitJobKind::PushTag {
            remote: remote.to_string(),
            tags: Vec::new(),
            all: true,
            credentials,
        });
    }

    pub fn start_pull(&mut self, remote: &str, branch: &str, strategy: crate::git::PullStrategy) {
        let credentials = self.resolve_https_credentials(remote);
        self.start_job(GitJobKind::Pull {
            remote: remote.to_string(),
            branch: branch.to_string(),
            strategy,
            credentials,
        });
    }

    /// If the given remote points at an HTTPS URL whose host matches a
    /// connected provider account, look up the stored PAT / OAuth token
    /// from the secret store and return it packaged as
    /// [`HttpsCredentials`] for `jobs.rs` to inject via the inline
    /// credential helper.
    ///
    /// Returns `None` when:
    ///   * we can't read the remote URL
    ///   * it's an SSH URL (ssh-agent / configured keys handle auth)
    ///   * no account is connected for this host
    ///   * the secret store doesn't have a token for that account
    ///
    /// In all those cases the job falls through to plain `git`, which
    /// with `GIT_TERMINAL_PROMPT=0` will either succeed (public repo /
    /// osxkeychain helper / SSH key) or fail fast with an actionable
    /// error (rather than hanging on a TTY prompt forever).
    fn resolve_https_credentials(
        &self,
        remote: &str,
    ) -> Option<crate::git::jobs::HttpsCredentials> {
        // 1. Get the repo path from the currently-active workspace.
        let repo_path = match &self.view {
            View::Workspace(tabs) if !tabs.launcher_active => {
                tabs.current().repo.path().to_path_buf()
            }
            _ => return None,
        };
        self.resolve_https_credentials_for_repo_path(&repo_path, remote)
    }

    fn resolve_https_credentials_for_repo_path(
        &self,
        repo_path: &Path,
        remote: &str,
    ) -> Option<crate::git::jobs::HttpsCredentials> {
        // 2. Ask git for the URL. This is a tiny synchronous subprocess,
        //    but we only do it on the user's explicit push/pull/fetch
        //    click (not per frame), so the cost is fine.
        let url = crate::git::cli::run_line(repo_path, ["remote", "get-url", remote]).ok()?;

        // 3. Parse the host. SSH / file / relative URLs → None.
        let host = remote_host(&url)?;
        let remote_owner = crate::git_url::parse(&url).map(|parsed| parsed.owner);

        // 4. Find the right account. Priority:
        //    a) Per-repo explicit selection (Settings → Repository → Account)
        //    b) A single connected account whose provider kind matches the host
        //    c) For multi-account hosts, one whose username matches the remote owner
        //
        //    If multiple host matches remain ambiguous, fall back to the
        //    user's normal git credential flow instead of injecting a
        //    potentially wrong token for the wrong account.
        let repo_settings = self.config.repo_settings_for(&repo_path);
        let account = if let Some(slug) = &repo_settings.provider_account {
            // Explicit per-repo override — find by slug.
            self.config
                .provider_accounts
                .iter()
                .find(|acc| acc.id.slug() == *slug)
        } else {
            select_auto_provider_account(
                &self.config.provider_accounts,
                &host,
                remote_owner.as_deref(),
            )
        }?;

        // 5. Pull the token from the secret store (OS keychain or the
        //    file fallback).
        let token = self
            .secret_store
            .load_pat(&account.id)
            .ok()
            .flatten()
            .map(|s| {
                use secrecy::ExposeSecret;
                s.expose_secret().to_string()
            })?;

        // Username convention: for token-based HTTPS auth, git hosts
        // accept any non-empty string as the user — `x-access-token`
        // is the broadly-documented choice for GitHub and also works
        // on GitLab / Bitbucket / Gitea / Codeberg.
        Some(crate::git::jobs::HttpsCredentials {
            username: "x-access-token".into(),
            password: token,
        })
    }

    fn start_job(&mut self, kind: GitJobKind) {
        let repo_path = match &self.view {
            View::Workspace(tabs) if !tabs.launcher_active => {
                tabs.current().repo.path().to_path_buf()
            }
            _ => return,
        };
        self.start_job_for_repo_path(&repo_path, kind);
    }

    fn start_job_for_repo_path(&mut self, repo_path: &Path, kind: GitJobKind) {
        if !self.ensure_git_cli_for(&kind_action_label(&kind)) {
            return;
        }
        let Some(ws) = self.workspace_by_path_mut(repo_path) else {
            self.last_error = Some(format!("repository not open: {}", repo_path.display()));
            return;
        };
        if ws.active_job.is_some() {
            self.last_error = Some("another git job is already running".into());
            return;
        }
        let path = ws.repo.path().to_path_buf();
        ws.active_job = Some(GitJob::spawn(path, kind));
    }

    pub fn cancel_active_job(&mut self) {
        let View::Workspace(tabs) = &mut self.view else {
            return;
        };
        if tabs.launcher_active {
            return;
        }
        if let Some(job) = tabs.current().active_job.as_ref() {
            job.cancel();
        }
    }

    /// Poll the active background job; when done, integrate the result.
    fn poll_active_job(&mut self) {
        let (finished, scope) = {
            let View::Workspace(tabs) = &mut self.view else {
                return;
            };
            let ws = tabs.current_mut();
            let Some(job) = ws.active_job.as_ref() else {
                return;
            };
            match job.poll() {
                None => return, // still running
                Some(r) => (r, ws.graph_scope),
            }
        };

        // Remove the handle and act on the result.
        if let View::Workspace(tabs) = &mut self.view {
            let ws = tabs.current_mut();
            let label = ws
                .active_job
                .as_ref()
                .map(|j| j.label())
                .unwrap_or_default();
            ws.active_job = None;
            match finished {
                Ok(()) => {
                    self.hud = Some(Hud::new(format!("✓ {label}"), 1800));
                }
                Err(e) => {
                    let diagnostic = crate::git::classify_git_error(&e);
                    if matches!(diagnostic.kind, crate::git::GitErrorKind::Cancelled) {
                        self.hud = Some(Hud::new(diagnostic.user_message(&label), 1800));
                    } else {
                        if matches!(diagnostic.kind, crate::git::GitErrorKind::MissingBinary) {
                            self.refresh_git_capability();
                        }
                        self.last_error = Some(diagnostic.user_message(&label));
                    }
                }
            }
        }
        self.rebuild_graph(scope);
    }

    /// Restore to the BEFORE state of a specific journal entry — used by
    /// Panic Recovery to jump past a cluster of confusing operations.
    pub fn restore_to_entry(&mut self, entry_id: u64) {
        // Same async pipeline as undo/redo — keeps the journal cursor
        // logic in one place and prevents UI freezes during the restore
        // (which can be just as expensive as a regular undo on big repos).
        if let Err(e) = self.spawn_nav(NavRequest::RestoreToBefore { entry_id }) {
            self.last_error = Some(format!("restore failed: {e:#}"));
        }
        self.panic_modal_open = false;
        self.nav_history.clear();
    }

    /// Evict any `bytes://diff/...` URIs that were replaced since the last
    /// frame. Called after the other pollers so in-flight completions have
    /// had a chance to push their outgoing caches into the queue first.
    ///
    /// Without this, egui's image loader holds decoded GPU textures for
    /// every diff the user has ever looked at this session — tens of MB
    /// over a long review.
    fn drain_image_evictions(&mut self, ctx: &egui::Context) {
        let View::Workspace(tabs) = &mut self.view else {
            return;
        };
        for tab in &mut tabs.tabs {
            if tab.pending_image_evictions.is_empty() {
                continue;
            }
            for uri in tab.pending_image_evictions.drain(..) {
                ctx.forget_image(&uri);
            }
        }
    }

    /// Poll any in-flight diff computations across all tabs and drop
    /// results whose target commit has changed (user clicked somewhere
    /// else while the worker was still running).
    fn poll_diff_tasks(&mut self) {
        let profile = std::env::var("MERGEFOX_PROFILE_DIFF").is_ok();
        let View::Workspace(tabs) = &mut self.view else {
            return;
        };
        // Collect outcomes first to keep borrows tidy.
        let mut completed: Vec<(
            usize,
            gix::ObjectId,
            std::time::Duration,
            std::result::Result<crate::git::RepoDiff, String>,
        )> = Vec::new();
        for (idx, tab) in tabs.tabs.iter_mut().enumerate() {
            let Some(task) = tab.diff_task.as_ref() else {
                continue;
            };
            if let Ok(result) = task.rx.try_recv() {
                let oid = task.oid;
                let elapsed = task.started_at.elapsed();
                tab.diff_task = None;
                if profile {
                    tracing::debug!(
                        target: "mergefox::profile::click",
                        oid = %&oid.to_string()[..7],
                        elapsed_us = elapsed.as_micros() as u64,
                        "click spawn→result"
                    );
                }
                completed.push((idx, oid, elapsed, result));
            }
        }
        for (tab_idx, oid, _elapsed, result) in completed {
            // We process each completed result in three stages. Lexical
            // scoping keeps the mutable borrow of the tab short so we can
            // re-borrow the workspace after (to spawn a follow-up task for
            // any queued click).
            let spawn_next: Option<gix::ObjectId>;
            {
                let View::Workspace(tabs) = &mut self.view else {
                    return;
                };
                let Some(tab) = tabs.tabs.get_mut(tab_idx) else {
                    continue;
                };
                match result {
                    Ok(diff) => {
                        let diff_arc = Arc::new(diff);
                        // Seed the LRU — even if this result is stale
                        // (user moved on), caching it means clicking
                        // back in a moment is instant.
                        tab.diff_cache.insert(oid, Arc::clone(&diff_arc));
                        // Install only if the user still has this commit
                        // selected. For stale results the cache insert
                        // above is the full payoff.
                        if tab.selected_commit == Some(oid) {
                            if let Some(idx) = tab.selected_file_idx {
                                if idx >= diff_arc.files.len() {
                                    tab.selected_file_idx = diff_arc.files.len().checked_sub(1);
                                }
                            }
                            tab.current_diff = Some(diff_arc);
                        }
                    }
                    Err(e) => {
                        if tab.selected_commit == Some(oid) {
                            tab.current_diff = None;
                            self.last_error = Some(format!("diff: {e}"));
                        }
                    }
                }
                // Decide whether there's a queued click to chase next.
                // We only chase it if it's different from what we just
                // computed (otherwise it's already fulfilled / cached).
                spawn_next = tab
                    .pending_diff_oid
                    .take()
                    .filter(|pending| *pending != oid);
            }
            if let Some(next_oid) = spawn_next {
                let View::Workspace(tabs) = &mut self.view else {
                    return;
                };
                let Some(tab) = tabs.tabs.get_mut(tab_idx) else {
                    continue;
                };
                // Cache hit? Install directly.
                if let Some(cached) = tab.diff_cache.get(&next_oid) {
                    if tab.selected_commit == Some(next_oid) {
                        tab.current_diff = Some(cached);
                    }
                } else if tab.selected_commit == Some(next_oid) {
                    crate::ui::main_panel::spawn_diff_worker(tab, next_oid, &self.egui_ctx);
                }
            }
        }
    }

    /// Poll any in-flight graph rebuilds across all tabs and install the
    /// new `CommitGraph` into the owning tab. Results whose scope no
    /// longer matches the tab's current `graph_scope` (user changed
    /// scope again while the worker was running) are discarded.
    fn poll_graph_tasks(&mut self) {
        let View::Workspace(tabs) = &mut self.view else {
            return;
        };
        let mut completed: Vec<(
            usize,
            GraphScope,
            std::result::Result<crate::git::CommitGraph, String>,
        )> = Vec::new();
        for (idx, tab) in tabs.tabs.iter_mut().enumerate() {
            let Some(task) = tab.graph_task.as_ref() else {
                continue;
            };
            if let Ok(result) = task.rx.try_recv() {
                let scope = task.scope;
                tab.graph_task = None;
                completed.push((idx, scope, result));
            }
        }
        for (tab_idx, scope, result) in completed {
            let View::Workspace(tabs) = &mut self.view else {
                return;
            };
            let Some(tab) = tabs.tabs.get_mut(tab_idx) else {
                continue;
            };
            if tab.graph_scope != scope {
                continue;
            }
            match result {
                Ok(graph) => {
                    let selected_commit = tab.selected_commit;
                    let mut graph_view = GraphView::new(Arc::new(graph));
                    if let Some(oid) = selected_commit {
                        graph_view.selected_row =
                            graph_view.graph.rows.iter().position(|row| row.oid == oid);
                    }
                    tab.graph_view = Some(graph_view);
                }
                Err(e) => {
                    self.last_error = Some(format!("graph rebuild: {e}"));
                }
            }
        }
    }

    /// Drain any LFS scan results that have arrived since last frame.
    ///
    /// We poll all open tabs (not just the active one) so a result that
    /// finishes while the user is on a different tab is still captured —
    /// otherwise the receiver would be dropped on tab switch and the user
    /// would silently lose the hint.
    fn poll_lfs_scan(&mut self) {
        let View::Workspace(tabs) = &mut self.view else {
            return;
        };
        for tab in &mut tabs.tabs {
            if let Some(rx) = &tab.lfs_scan.running {
                if let Ok(result) = rx.try_recv() {
                    tab.lfs_scan.result = Some(result);
                    tab.lfs_scan.running = None;
                }
            }
        }
    }
}

/// Spawn a background thread that walks HEAD's tree looking for tracked
/// blobs that should arguably move to Git LFS. The thread opens its own
/// repo handle on its own thread because `gix::Repository` stays in-process
/// from the UI thread.
///
/// The receiver lands in `WorkspaceState::lfs_scan.running`; the per-frame
/// `poll_lfs_scan` drains it and stores the result for the sidebar to
/// render. Errors are swallowed (logged) — a failing scan shouldn't hold
/// up repo open or block UI updates.
fn spawn_lfs_scan(repo_path: &Path) -> LfsScanState {
    let (tx, rx) = std::sync::mpsc::channel();
    let path = repo_path.to_path_buf();
    std::thread::spawn(move || {
        let result = crate::git::lfs::scan(&path, crate::git::lfs::DEFAULT_MIN_SIZE);
        match result {
            Ok(r) => {
                let _ = tx.send(r);
            }
            Err(e) => {
                tracing::warn!(error = %e, "lfs scan failed");
                // Still send an empty result so the UI knows the scan
                // finished (otherwise sidebar would show "scanning…"
                // forever).
                let _ = tx.send(crate::git::LfsScanResult {
                    head_oid: None,
                    candidates: Vec::new(),
                    truncated: false,
                    total_bytes_scanned: 0,
                });
            }
        }
    });
    LfsScanState {
        running: Some(rx),
        result: None,
        dismissed: false,
    }
}

#[allow(dead_code)]
fn _assert_clone(_: &JournalEntry) {}

// Note: the standalone `ensure_working_clean_or_stash` helper that used
// to live here was removed in favor of `Repo::auto_stash_if_dirty`. The
// Repo version drops INCLUDE_UNTRACKED (force-checkout doesn't touch
// untracked files, so stashing them was wasted I/O on game-engine-style
// repos with huge untracked artifact directories), and adds a pre-flight
// size guard so multi-hundred-MB textures can't silently freeze the UI
// for minutes. Single source of truth now.

enum StalePathKind {
    /// Path doesn't exist at all.
    Missing,
    /// Directory exists and contains `.git/` but it's an empty stub — the
    /// footprint of a clone that died before creating any refs or objects.
    PartialClone,
    /// Something else went wrong; caller falls back to the direct `git` CLI
    /// error string.
    Other,
}

fn classify_stale_path(path: &Path) -> StalePathKind {
    if !path.exists() {
        return StalePathKind::Missing;
    }
    let git_dir = path.join(".git");
    if !git_dir.exists() {
        return StalePathKind::Other;
    }
    // Classic failed-clone signature: `.git/HEAD` missing. A healthy repo
    // always has HEAD; an aborted clone often leaves only `logs/` and a
    // few config skeletons behind.
    if !git_dir.join("HEAD").exists() {
        return StalePathKind::PartialClone;
    }
    StalePathKind::Other
}

/// Full-window loading view rendered while `View::OpeningRepo` is active.
///
/// Reads the current stage label from the worker-shared `Arc<Mutex<String>>`
/// and shows a spinner + elapsed time. Also requests a repaint every frame
/// so the elapsed counter stays live — without that, egui's idle behaviour
/// would leave the screen frozen on the initial label.
fn render_opening_repo(ctx: &egui::Context, app: &mut MergeFoxApp) {
    let (path_display, stage, elapsed) = match &app.view {
        View::OpeningRepo(state) => (
            state.path.display().to_string(),
            state
                .label
                .lock()
                .map(|g| g.clone())
                .unwrap_or_else(|_| "Opening…".into()),
            state.started_at.elapsed().as_secs(),
        ),
        _ => return,
    };

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.3);
            ui.heading("Opening repository");
            ui.add_space(8.0);
            ui.weak(&path_display);
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(egui::RichText::new(&stage).size(14.0));
                ui.weak(format!("({}s)", elapsed));
            });
            ui.add_space(12.0);
            ui.weak(
                "Large repositories (Linux kernel, monorepos) may take a moment \
                 while refs and the commit graph are built. The UI stays \
                 responsive — you can cancel by closing the window.",
            );
        });
    });

    // Keep the elapsed counter ticking without user input.
    ctx.request_repaint_after(std::time::Duration::from_millis(200));
}

fn short_sha(oid: &gix::ObjectId) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}

/// Spawn a worker to rebuild the commit graph for `scope`, stashing the
/// receiver in `ws.graph_task`. The previous `graph_view` (if any) stays
/// on screen until the new result arrives via `poll_graph_tasks`, so the
/// UI stays responsive even on kernel-scale repos where `CommitGraph::build`
/// walks for hundreds of milliseconds.
///
/// The worker opens its own `gix::Repository` (gix repos are `Send` but
/// we still prefer per-thread handles so main-thread reads and
/// background graph builds never contend on a mutex). This is cheap
/// after the initial open: git's ref / pack indexes are already warm
/// in the OS page cache.
fn spawn_graph_task(ws: &mut WorkspaceState, scope: GraphScope, ctx: &egui::Context) {
    let (tx, rx) = std::sync::mpsc::channel();
    let repo_path = ws.repo.path().to_path_buf();
    let ctx_clone = ctx.clone();
    std::thread::spawn(move || {
        let result = (|| -> anyhow::Result<crate::git::CommitGraph> {
            let repo = crate::git::Repo::open(&repo_path)?;
            repo.build_graph(scope)
        })()
        .map_err(|e| format!("{e:#}"));
        let _ = tx.send(result);
        ctx_clone.request_repaint();
    });
    ws.graph_task = Some(GraphTask { scope, rx });
}

fn reset_forge_state(forge: &mut ForgeState) {
    forge.repo = None;
    forge.pull_requests.clear();
    forge.issues.clear();
    forge.selected = None;
    forge.pr_modal = None;
    forge.issue_modal = None;
    forge.loaded_once = false;
    forge.loading = false;
    forge.last_error = None;
}

fn pull_request_head_status(repo: &Repo, head_branch: Option<&str>) -> (bool, Option<String>) {
    let Some(head_branch) = head_branch else {
        return (
            false,
            Some("Check out a local branch before creating a pull request.".into()),
        );
    };

    let upstream = repo.list_branches(false).ok().and_then(|branches| {
        branches
            .into_iter()
            .find(|branch| !branch.is_remote && branch.name == head_branch)
            .and_then(|branch| branch.upstream)
    });

    if upstream.is_some() {
        (true, None)
    } else {
        (
            false,
            Some(format!(
                "Push `{head_branch}` or set its upstream before creating a pull request."
            )),
        )
    }
}

fn build_rebase_steps(items: &[RebasePlanItem]) -> Result<Vec<RebaseSessionStep>> {
    let mut steps = Vec::with_capacity(items.len());
    let mut last_kept_message: Option<String> = None;

    for item in items {
        let edited = item.edited_message.trim();
        let message = if edited.is_empty() {
            item.original_message.trim().to_owned()
        } else {
            item.edited_message.clone()
        };

        match item.action {
            RebaseAction::Pick => {
                last_kept_message = Some(message.clone());
                steps.push(RebaseSessionStep {
                    oid: item.oid,
                    action: RebaseAction::Pick,
                    message,
                });
            }
            RebaseAction::Reword => {
                last_kept_message = Some(message.clone());
                steps.push(RebaseSessionStep {
                    oid: item.oid,
                    action: RebaseAction::Reword,
                    message,
                });
            }
            RebaseAction::Squash => {
                let previous = last_kept_message
                    .clone()
                    .context("the first kept commit cannot use squash")?;
                let combined = combine_rebase_messages(&previous, &message);
                last_kept_message = Some(combined.clone());
                steps.push(RebaseSessionStep {
                    oid: item.oid,
                    action: RebaseAction::Squash,
                    message: combined,
                });
            }
            RebaseAction::Fixup => {
                // Fixup rolls into the previous kept commit AND
                // discards this commit's message. The rebase execution
                // path already handles `Squash` with a "combined
                // message that's now the final commit message" model —
                // we produce the same shape but the message carried
                // forward is the PREVIOUS message unchanged, so this
                // commit's text never lands.
                let previous = last_kept_message
                    .clone()
                    .context("the first kept commit cannot use fixup")?;
                last_kept_message = Some(previous.clone());
                // Execution-side treats this as Squash with the previous
                // message — `finish_pending_pick_squash(previous)` is a
                // correct reduction. We still record `Fixup` so the
                // session log shows the user's chosen semantics.
                steps.push(RebaseSessionStep {
                    oid: item.oid,
                    action: RebaseAction::Fixup,
                    message: previous,
                });
            }
            RebaseAction::Drop => steps.push(RebaseSessionStep {
                oid: item.oid,
                action: RebaseAction::Drop,
                message,
            }),
        }
    }

    if !steps.iter().any(|step| step.action != RebaseAction::Drop) {
        bail!("select at least one commit to keep");
    }

    Ok(steps)
}

fn combine_rebase_messages(previous: &str, current: &str) -> String {
    let previous = previous.trim();
    let current = current.trim();
    match (previous.is_empty(), current.is_empty()) {
        (true, true) => String::new(),
        (true, false) => current.to_owned(),
        (false, true) => previous.to_owned(),
        (false, false) => format!("{previous}\n\n{current}"),
    }
}

impl eframe::App for MergeFoxApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Per-frame timing probe. `MERGEFOX_PROFILE_FRAMES=1` dumps the
        // duration of every frame plus the gap since the last frame to
        // stderr. Lets us tell apart "frame takes too long" (paint cost)
        // from "frames aren't happening" (idle scheduler).
        let frame_profile = std::env::var("MERGEFOX_PROFILE_FRAMES").is_ok();
        let frame_t0 = std::time::Instant::now();
        if frame_profile {
            use std::sync::atomic::{AtomicU64, Ordering};
            static LAST_FRAME_NANOS: AtomicU64 = AtomicU64::new(0);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let last = LAST_FRAME_NANOS.swap(now, Ordering::Relaxed);
            let gap_ms = if last == 0 {
                0
            } else {
                (now - last) / 1_000_000
            };
            tracing::trace!(target: "mergefox::profile::frame", gap_since_last_ms = gap_ms, "frame start");
        }
        ui::theme::apply(ctx, &self.config.theme);

        // One-shot startup notifications. Fired on the first `update`
        // frame so the notification center has been rendered at least
        // once and the toast isn't competing with layout settling.
        // Re-checking on each frame would spam the notification queue
        // if git goes missing mid-session — explicit "check once" flag.
        if !self.startup_notified {
            self.startup_notified = true;
            self.notify_git_probe_on_startup();
        }
        handle_hotkeys(ctx, self);
        self.poll_opening_repo();
        self.poll_clone_jobs();
        self.poll_remote_repo_jobs();
        self.poll_publish_remote_modal_tasks();
        self.poll_active_job();
        self.poll_forge_tasks();
        self.poll_lfs_scan();
        self.poll_nav_tasks();
        self.poll_diff_tasks();
        self.poll_graph_tasks();
        self.drain_image_evictions(ctx);
        self.poll_working_tree_changes();

        match &mut self.view {
            View::Welcome(_) => ui::welcome::show(ctx, self),
            View::OpeningRepo(_) => render_opening_repo(ctx, self),
            View::Workspace(tabs) if tabs.launcher_active => {
                ui::tabs::show(ctx, self);
                ui::welcome::show(ctx, self);
            }
            View::Workspace(_) => {
                ui::top_bar::show(ctx, self);
                ui::tabs::show(ctx, self);
                ui::sidebar::show(ctx, self);
                ui::diff_view::show(ctx, self);
                ui::main_panel::show(ctx, self);
            }
        }

        ui::hud::show(ctx, self);
        ui::notifications::show(ctx, self);
        ui::panic_recovery::show(ctx, self);
        ui::commit_modal::show(ctx, self);
        ui::prompt::show(ctx, self);
        ui::columns::show(ctx, self);
        ui::activity_log::show(ctx, self);
        ui::reflog::show(ctx, self);
        ui::shortcuts::show(ctx, self);
        ui::palette::show(ctx, self);
        ui::blame::show(ctx, self);
        ui::bisect::show(ctx, self);
        ui::settings::show(ctx, self);
        ui::publish_remote::show(ctx, self);
        ui::forge::show(ctx, self);
        ui::rebase::show(ctx, self);
        ui::conflicts::show(ctx, self);

        if self.clone_in_progress() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if self.hud.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if self.provider_oauth_start_task.is_some() || self.provider_oauth_poll_task.is_some() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        if self.remote_repo_refresh_in_progress() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        if self
            .publish_remote_modal
            .as_ref()
            .is_some_and(|modal| modal.owners_task.is_some() || modal.create_task.is_some())
        {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        if self.forge_refresh_task.is_some()
            || self.forge_create_pr_task.is_some()
            || self.forge_create_issue_task.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        if let View::Workspace(tabs) = &self.view {
            let ws = tabs.current();
            ctx.request_repaint_after(WORKING_TREE_POLL_INTERVAL);
            if ws.active_job.is_some() {
                ctx.request_repaint_after(Duration::from_millis(120));
            }
            // CRITICAL: if a background diff / graph / nav task is in
            // flight, egui must be told to wake up soon — otherwise it
            // idles and the finished result won't land until the user
            // nudges the mouse or types a key. That's the whole "clicks
            // feel super laggy" symptom.
            //
            // We poll fast (every 16 ms ≈ 60 Hz) while a task is pending
            // so the user sees the diff appear as soon as the worker
            // finishes. Once the task clears, the app goes back to idle.
            if ws.diff_task.is_some() || ws.graph_task.is_some() || ws.nav_task.is_some() {
                ctx.request_repaint_after(Duration::from_millis(16));
            }
        }
        // DIAGNOSTIC: force continuous 60 Hz rendering. If lag clears,
        // the problem is the idle scheduler (request_repaint from
        // background threads not waking the event loop). If lag stays,
        // the problem is somewhere inside the per-frame work.
        if std::env::var("MERGEFOX_FORCE_CONTINUOUS").is_ok() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
        if frame_profile {
            tracing::trace!(
                target: "mergefox::profile::frame",
                cost_us = frame_t0.elapsed().as_micros() as u64,
                "frame end"
            );
        }
    }

    // Note: on_event is not a member of eframe::App trait in current version.
    // File drop handling should be implemented via raw_window_event instead.
    // fn on_event(&mut self, _ctx: &egui::Context, event: &egui::Event) -> bool {
    //     match event {
    //         egui::Event::DroppedFile(egui::DroppedFile { path: Some(path), .. }) => {
    //             // 폴더 드롭 - Git 저장소 열기
    //             if path.is_dir() {
    //                 self.open_repo(path);
    //                 return true;
    //             }
    //             // 파일 드롭 - .git 폴더 또는 일반 파일이 포함된 폴더에서 저장소 검색
    //             if let Some(parent) = path.parent() {
    //                 if parent.join(".git").exists() || parent.join("HEAD").exists() {
    //                     self.open_repo(parent);
    //                     return true;
    //                 }
    //             }
    //         }
    //         _ => {}
    //     }
    //     false
    // }
}

fn handle_hotkeys(ctx: &egui::Context, app: &mut MergeFoxApp) {
    // We read modifiers + key directly (instead of `consume_shortcut`) so
    // Cmd+Z and Cmd+Shift+Z are *strictly* disambiguated. `consume_shortcut`
    // for Cmd+Z was eating Cmd+Shift+Z events too on macOS, which flipped
    // redo presses into undo ("nothing to undo").
    // `?` pulls up the shortcuts cheat-sheet. We suppress it when a
    // text field owns keyboard focus (commit message, settings inputs)
    // so typing literal `?` into a message still works — the rule
    // matches the one we use for `Esc` inside the Settings modal.
    let shortcuts_hotkey_allowed = !ctx.wants_keyboard_input();

    let (undo, redo, panic_key, next_tab, prev_tab, close_tab, open_reflog, open_shortcuts, open_palette) = ctx.input_mut(|i| {
        let z = i.key_pressed(egui::Key::Z);
        let esc = i.key_pressed(egui::Key::Escape);
        let tab_k = i.key_pressed(egui::Key::Tab);
        let w_k = i.key_pressed(egui::Key::W);
        let r_k = i.key_pressed(egui::Key::R);
        let k_k = i.key_pressed(egui::Key::K);
        let questionmark =
            i.key_pressed(egui::Key::Questionmark) || i.key_pressed(egui::Key::Slash);
        let m = i.modifiers;
        // On macOS, `command` already represents the Cmd key; we don't
        // require anything special about ctrl here.
        let cmd_only = m.command && !m.shift && !m.alt;
        let cmd_shift = m.command && m.shift && !m.alt;
        // Ctrl+Tab is the portable "cycle tab" shortcut even on macOS —
        // browsers and terminal multiplexers use it. `ctrl` here is the
        // literal Control key; `command` is Cmd on mac / Win on windows.
        let ctrl_only = m.ctrl && !m.shift && !m.alt && !m.command;
        let ctrl_shift = m.ctrl && m.shift && !m.alt && !m.command;
        let undo = z && cmd_only;
        let redo = z && cmd_shift;
        let panic_key = esc && cmd_shift;
        let next_tab = tab_k && ctrl_only;
        let prev_tab = tab_k && ctrl_shift;
        let close_tab = w_k && cmd_only;
        // Cmd+Shift+R — "I need a recovery lifeline". The reflog is the
        // universal safety net for destructive ops (hard reset, amend,
        // force push, rebase gone wrong); this shortcut matches the
        // Refresh conventions on web browsers (Cmd+Shift+R) while
        // reading as "recover" in our context. Users in-panic shouldn't
        // have to hunt for a menu item.
        let open_reflog = r_k && cmd_shift;
        // `?` (also `Shift+/` on US layouts) brings up the cheatsheet.
        // Must have no other modifiers so it doesn't clash with future
        // Cmd+? bindings. Suppressed while a text field has focus —
        // see `shortcuts_hotkey_allowed` above.
        let open_shortcuts = questionmark
            && !m.ctrl
            && !m.alt
            && !m.command
            && shortcuts_hotkey_allowed;
        // ⌘K / Ctrl+K — command palette. Always available, even while
        // a text field is focused (that's the whole point of the
        // palette; users hit it mid-typing to jump around).
        let open_palette = k_k && cmd_only;
        // Consume the events so textfields / other widgets don't also react.
        if undo || redo {
            i.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::Z,
                        pressed: true,
                        ..
                    }
                )
            });
        }
        if panic_key {
            i.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::Escape,
                        pressed: true,
                        ..
                    }
                )
            });
        }
        if next_tab || prev_tab {
            i.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::Tab,
                        pressed: true,
                        ..
                    }
                )
            });
        }
        if close_tab {
            i.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::W,
                        pressed: true,
                        ..
                    }
                )
            });
        }
        if open_reflog {
            i.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::R,
                        pressed: true,
                        ..
                    }
                )
            });
        }
        if open_shortcuts {
            i.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::Questionmark | egui::Key::Slash,
                        pressed: true,
                        ..
                    }
                )
            });
        }
        if open_palette {
            i.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::K,
                        pressed: true,
                        ..
                    }
                )
            });
        }
        (
            undo,
            redo,
            panic_key,
            next_tab,
            prev_tab,
            close_tab,
            open_reflog,
            open_shortcuts,
            open_palette,
        )
    });

    if redo {
        // Check redo first — if both flags somehow ended up true (shouldn't,
        // with the strict match above, but belt-and-braces), prefer redo.
        app.redo();
    } else if undo {
        app.undo();
    }
    if panic_key {
        app.open_panic_recovery();
    }
    if prev_tab {
        app.prev_tab();
    } else if next_tab {
        app.next_tab();
    }
    if close_tab {
        app.close_active_tab();
    }
    if open_reflog {
        // Only meaningful inside a workspace — on welcome / opening-repo
        // the reflog window has no repo to read from.
        if matches!(app.view, View::Workspace(_)) {
            app.reflog_open = true;
        }
    }
    if open_shortcuts {
        // Toggle — pressing `?` a second time closes the cheatsheet,
        // matching the "press same key to dismiss" mental model.
        app.shortcuts_open = !app.shortcuts_open;
    }
    if open_palette {
        // Toggle — ⌘K again closes the palette without executing an
        // action. Resetting query + selection keeps the next open
        // from inheriting stale input.
        app.palette_open = !app.palette_open;
        if !app.palette_open {
            app.palette_query.clear();
            app.palette_selected = 0;
        }
    }
}

/// Extract the hostname from a git remote URL.
///
/// Accepts:
///   * `https://github.com/owner/repo.git` → `Some("github.com")`
///   * `http://host:8080/foo` → `Some("host")`
/// Rejects:
///   * `git@github.com:owner/repo.git` (SSH — caller treats as "no HTTPS creds")
///   * relative / `file://` / malformed URLs
fn remote_host(url: &str) -> Option<String> {
    let scheme_end = url.find("://")?;
    let scheme = &url[..scheme_end];
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let rest = &url[scheme_end + 3..];
    // Strip userinfo if present (`user:pass@host/...` — rare but legal).
    let rest = rest.splitn(2, '@').last().unwrap_or(rest);
    // Host ends at the first `/`, `:`, or end-of-string.
    let host_end = rest
        .find(|c: char| c == '/' || c == ':')
        .unwrap_or(rest.len());
    let host = &rest[..host_end];
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Does a connected-account's provider kind match this remote host?
///
/// We match the well-known providers by their canonical hostname and
/// fall back to string equality for self-hosted Gitea / generic entries.
fn provider_matches_host(kind: &crate::providers::ProviderKind, host: &str) -> bool {
    use crate::providers::ProviderKind;
    let host = host.to_ascii_lowercase();
    match kind {
        ProviderKind::GitHub => host == "github.com",
        ProviderKind::GitLab => host == "gitlab.com",
        ProviderKind::Bitbucket => host == "bitbucket.org",
        ProviderKind::AzureDevOps => {
            host.ends_with("dev.azure.com") || host.ends_with("visualstudio.com")
        }
        ProviderKind::Codeberg => host == "codeberg.org",
        ProviderKind::Gitea { instance } => {
            // `instance` is scheme+host, e.g. https://git.example.com
            instance
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .split('/')
                .next()
                .map(|h| h.to_ascii_lowercase() == host)
                .unwrap_or(false)
        }
        ProviderKind::Generic { host: h } => h.to_ascii_lowercase() == host,
    }
}

fn kind_action_label(kind: &GitJobKind) -> String {
    match kind {
        GitJobKind::Fetch { remote, .. } => format!("Fetching '{remote}'"),
        GitJobKind::Push { remote, force, .. } => {
            if *force {
                format!("Force-pushing to '{remote}'")
            } else {
                format!("Pushing to '{remote}'")
            }
        }
        GitJobKind::Pull {
            remote,
            branch,
            strategy,
            ..
        } => {
            let mode = match strategy {
                crate::git::PullStrategy::Merge => "merge",
                crate::git::PullStrategy::Rebase => "rebase",
                crate::git::PullStrategy::FastForwardOnly => "fast-forward only",
            };
            format!("Pulling {remote}/{branch} ({mode})")
        }
        GitJobKind::PushTag {
            remote, tags, all, ..
        } => {
            if *all {
                format!("Pushing all tags to '{remote}'")
            } else if tags.len() == 1 {
                format!("Pushing tag '{}' to '{remote}'", tags[0])
            } else {
                format!("Pushing {} tags to '{remote}'", tags.len())
            }
        }
    }
}

impl RepoExternalSnapshot {
    fn requires_graph_refresh(&self, previous: &Self) -> bool {
        self.head_name != previous.head_name
            || self.head_oid != previous.head_oid
            || self.packed_refs_mtime_ns != previous.packed_refs_mtime_ns
            || self.refs_heads_mtime_ns != previous.refs_heads_mtime_ns
            || self.refs_remotes_mtime_ns != previous.refs_remotes_mtime_ns
            || self.fetch_head_mtime_ns != previous.fetch_head_mtime_ns
            || self.stash_log_mtime_ns != previous.stash_log_mtime_ns
    }
}

fn capture_repo_external_snapshot(repo: &Repo) -> RepoExternalSnapshot {
    let git_dir = repo.git_dir();
    RepoExternalSnapshot {
        head_name: repo.head_name(),
        head_oid: repo.head_oid(),
        head_file_mtime_ns: file_mtime_ns(&git_dir.join("HEAD")),
        packed_refs_mtime_ns: file_mtime_ns(&git_dir.join("packed-refs")),
        refs_heads_mtime_ns: dir_tree_mtime_ns(&git_dir.join("refs").join("heads")),
        refs_remotes_mtime_ns: dir_tree_mtime_ns(&git_dir.join("refs").join("remotes")),
        fetch_head_mtime_ns: file_mtime_ns(&git_dir.join("FETCH_HEAD")),
        stash_log_mtime_ns: file_mtime_ns(&git_dir.join("logs").join("refs").join("stash")),
        index_mtime_ns: file_mtime_ns(&git_dir.join("index")),
    }
}

fn file_mtime_ns(path: &Path) -> Option<u128> {
    std::fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(system_time_to_nanos)
}

fn dir_tree_mtime_ns(path: &Path) -> Option<u128> {
    let mut newest = file_mtime_ns(path);
    let Ok(entries) = std::fs::read_dir(path) else {
        return newest;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        let child_mtime = if child.is_dir() {
            dir_tree_mtime_ns(&child)
        } else {
            file_mtime_ns(&child)
        };
        newest = match (newest, child_mtime) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (None, Some(b)) => Some(b),
            (some, None) => some,
        };
    }
    newest
}

fn system_time_to_nanos(time: SystemTime) -> Option<u128> {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos())
}

pub fn default_remote_name(ws: &WorkspaceState, config: &Config) -> String {
    let settings = config.repo_settings_for(ws.repo.path());
    let remotes: Vec<String> = ws
        .repo_ui_cache
        .as_ref()
        .map(|c| c.remotes.clone())
        .unwrap_or_default();
    let upstream_remote = head_upstream(ws).map(|(remote, _branch)| remote);
    select_default_remote_name(
        settings.default_remote.as_deref(),
        upstream_remote.as_deref(),
        &remotes,
    )
    .unwrap_or_else(|| "origin".to_string())
}

pub fn tracked_upstream_for_branch(
    ws: &WorkspaceState,
    branch_name: &str,
) -> Option<(String, String)> {
    ws.repo_ui_cache
        .as_ref()?
        .branches
        .iter()
        .find(|branch| !branch.is_remote && branch.name == branch_name)
        .and_then(|branch| branch.upstream.as_deref())
        .and_then(parse_upstream_ref)
}

fn head_upstream(ws: &WorkspaceState) -> Option<(String, String)> {
    ws.repo_ui_cache
        .as_ref()?
        .branches
        .iter()
        .find(|branch| branch.is_head && !branch.is_remote)
        .and_then(|branch| parse_upstream_ref(branch.upstream.as_deref()?))
}

pub fn parse_upstream_ref(upstream: &str) -> Option<(String, String)> {
    let (remote, branch) = upstream.split_once('/')?;
    if remote.is_empty() || branch.is_empty() {
        None
    } else {
        Some((remote.to_string(), branch.to_string()))
    }
}

fn upstream_remote_name(upstream: &str) -> Option<&str> {
    let (remote, _branch) = upstream.split_once('/')?;
    if remote.is_empty() {
        None
    } else {
        Some(remote)
    }
}

fn select_default_remote_name(
    preferred_remote: Option<&str>,
    upstream_remote: Option<&str>,
    remotes: &[String],
) -> Option<String> {
    preferred_remote
        .filter(|preferred| remotes.iter().any(|name| name == preferred))
        .map(str::to_string)
        .or_else(|| {
            upstream_remote
                .filter(|upstream| remotes.iter().any(|name| name == upstream))
                .map(str::to_string)
        })
        .or_else(|| remotes.first().cloned())
}

fn select_auto_provider_account<'a>(
    accounts: &'a [crate::providers::ProviderAccount],
    host: &str,
    remote_owner: Option<&str>,
) -> Option<&'a crate::providers::ProviderAccount> {
    let mut host_matches = accounts
        .iter()
        .filter(|acc| provider_matches_host(&acc.id.kind, host));
    let first = host_matches.next()?;
    let second = host_matches.next();
    if second.is_none() {
        return Some(first);
    }

    let remote_owner = remote_owner?;
    accounts
        .iter()
        .filter(|acc| provider_matches_host(&acc.id.kind, host))
        .find(|acc| acc.id.username.eq_ignore_ascii_case(remote_owner))
}

#[cfg(test)]
mod tests {
    use super::{
        provider_matches_host, remote_host, select_auto_provider_account,
        select_default_remote_name, upstream_remote_name,
    };
    use crate::providers::{AccountId, AuthMethod, ProviderAccount, ProviderKind};

    fn github_account(username: &str) -> ProviderAccount {
        ProviderAccount {
            id: AccountId {
                kind: ProviderKind::GitHub,
                username: username.to_string(),
            },
            display_name: username.to_string(),
            avatar_url: None,
            method: AuthMethod::OAuth,
            created_unix: 0,
        }
    }

    #[test]
    fn remote_host_parses_https_host() {
        assert_eq!(
            remote_host("https://github.com/openai/example.git").as_deref(),
            Some("github.com")
        );
    }

    #[test]
    fn provider_matches_github_host() {
        assert!(provider_matches_host(&ProviderKind::GitHub, "github.com"));
        assert!(!provider_matches_host(&ProviderKind::GitHub, "gitlab.com"));
    }

    #[test]
    fn auto_provider_selects_single_host_match() {
        let accounts = vec![github_account("alice")];
        let selected = select_auto_provider_account(&accounts, "github.com", None)
            .map(|acc| acc.id.username.as_str());
        assert_eq!(selected, Some("alice"));
    }

    #[test]
    fn auto_provider_prefers_remote_owner_when_multiple_accounts_exist() {
        let accounts = vec![github_account("alice"), github_account("bob")];
        let selected = select_auto_provider_account(&accounts, "github.com", Some("bob"))
            .map(|acc| acc.id.username.as_str());
        assert_eq!(selected, Some("bob"));
    }

    #[test]
    fn auto_provider_returns_none_when_multiple_accounts_are_ambiguous() {
        let accounts = vec![github_account("alice"), github_account("bob")];
        assert!(select_auto_provider_account(&accounts, "github.com", Some("org-name")).is_none());
        assert!(select_auto_provider_account(&accounts, "github.com", None).is_none());
    }

    #[test]
    fn upstream_remote_name_extracts_remote() {
        assert_eq!(upstream_remote_name("tradeosx/main"), Some("tradeosx"));
        assert_eq!(upstream_remote_name("origin/feature/foo"), Some("origin"));
        assert_eq!(upstream_remote_name("main"), None);
    }

    #[test]
    fn default_remote_prefers_explicit_setting_when_usable() {
        let remotes = vec!["tradeosx".to_string(), "backup".to_string()];
        let selected = select_default_remote_name(Some("backup"), Some("tradeosx"), &remotes);
        assert_eq!(selected.as_deref(), Some("backup"));
    }

    #[test]
    fn default_remote_falls_back_to_upstream_before_first_remote() {
        let remotes = vec!["backup".to_string(), "tradeosx".to_string()];
        let selected = select_default_remote_name(None, Some("tradeosx"), &remotes);
        assert_eq!(selected.as_deref(), Some("tradeosx"));
    }

    #[test]
    fn default_remote_ignores_missing_preference_and_uses_first_available() {
        let remotes = vec!["tradeosx".to_string()];
        let selected = select_default_remote_name(Some("origin"), Some("origin"), &remotes);
        assert_eq!(selected.as_deref(), Some("tradeosx"));
    }
}
