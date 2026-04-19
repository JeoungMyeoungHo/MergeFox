use crate::actions::{CommitAction, ResetMode};
use crate::app::{
    default_remote_name, tracked_upstream_for_branch, MergeFoxApp, SelectedFileView, View,
};
use crate::config::Config;
use crate::git::GraphScope;
use crate::journal::{self, Operation};
use crate::ui::prompt::{self, PendingPrompt};

/// Buttons / hotkeys at the top of the main panel can request the app
/// to step the undo journal. We collect these as flags during the UI
/// closure and apply them after the closure releases its borrow.
#[derive(Default)]
struct PanelIntent {
    new_scope: Option<GraphScope>,
    action: Option<CommitAction>,
    undo: bool,
    redo: bool,
    open_panic: bool,
    open_commit: bool,
    open_rebase: bool,
    open_columns: bool,
    open_activity_log: bool,
}

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    let mut intent = PanelIntent::default();
    // Snapshot panic state up-front so the UI closure doesn't re-borrow `app`.
    let panic_active = app.panic_detector_active();

    let mut commit_clicked: Option<gix::ObjectId> = None;
    egui::CentralPanel::default().show(ctx, |ui| {
        let View::Workspace(tabs) = &mut app.view else {
            return;
        };
        let ws = tabs.current_mut();

        if crate::ui::diff_view::has_selected_file(ws) {
            crate::ui::diff_view::show_selected_file_center(ui, ws);
            return;
        }

        // ---------- toolbar ----------
        ui.horizontal(|ui| {
            ui.label("Scope:");
            for opt in [
                GraphScope::CurrentBranch,
                GraphScope::AllLocal,
                GraphScope::AllRefs,
            ] {
                if ui
                    .selectable_label(ws.graph_scope == opt, opt.label())
                    .clicked()
                    && ws.graph_scope != opt
                {
                    intent.new_scope = Some(opt);
                }
            }
            ui.separator();
            if ui.button("🔄 Refresh").clicked() {
                intent.new_scope = Some(ws.graph_scope);
            }
            if ui.button("📝 Commit…").clicked() {
                intent.open_commit = true;
            }
            ui.add_enabled_ui(
                matches!(ws.repo.state(), crate::git::RepoState::Clean),
                |ui| {
                    if ui
                        .button("⎇ Rebase…")
                        .on_hover_text("Plan an interactive rebase for the current branch")
                        .clicked()
                    {
                        intent.open_rebase = true;
                    }
                },
            );

            ui.separator();
            if ui
                .button("⚙ Columns")
                .on_hover_text("Show/hide columns (Branch, Graph, Message, Author, Date, Sha)")
                .clicked()
            {
                intent.open_columns = true;
            }
            if ui
                .button("📜 Log")
                .on_hover_text("Open MCP activity log — recent git operations for debugging")
                .clicked()
            {
                intent.open_activity_log = true;
            }

            ui.separator();

            // Undo/Redo
            let (can_u, can_r, peek_u, peek_r) = match ws.journal.as_ref() {
                Some(j) => (
                    j.cursor.is_some(),
                    j.peek_redo().is_some(),
                    j.peek_undo().map(|e| e.operation.label()),
                    j.peek_redo().map(|e| e.operation.label()),
                ),
                None => (false, false, None, None),
            };
            ui.add_enabled_ui(can_u, |ui| {
                let btn = ui.button("↶ Undo");
                let btn = if let Some(s) = &peek_u {
                    btn.on_hover_text(format!("Cmd+Z — {s}"))
                } else {
                    btn.on_hover_text("Cmd+Z")
                };
                if btn.clicked() {
                    intent.undo = true;
                }
            });
            ui.add_enabled_ui(can_r, |ui| {
                let btn = ui.button("↷ Redo");
                let btn = if let Some(s) = &peek_r {
                    btn.on_hover_text(format!("Cmd+Shift+Z — {s}"))
                } else {
                    btn.on_hover_text("Cmd+Shift+Z")
                };
                if btn.clicked() {
                    intent.redo = true;
                }
            });

            // Panic indicator — shown only when detection flags recent spam.
            if panic_active {
                ui.separator();
                if ui
                    .button(egui::RichText::new("🆘 Recovery").color(egui::Color32::YELLOW))
                    .on_hover_text("Lots of undo/redo lately — open recovery options")
                    .clicked()
                {
                    intent.open_panic = true;
                }
            }
        });

        ui.separator();

        // Banner: shown whenever a commit is selected. Previously we only
        // showed it once `current_diff` was ready, which made it blink in
        // / out as the async diff computation finished — every commit
        // click shifted the graph up/down by the banner's height. Now
        // the banner is present from the moment a commit is selected;
        // text flips between "computing…" and "select a file" based on
        // whether the diff has arrived. Total height stays stable, so
        // no more jitter.
        if ws.selected_commit.is_some() || ws.selected_working_tree {
            let msg = if ws.selected_working_tree {
                "Select a file from the right panel to open its diff or file view."
            } else if ws.current_diff.is_some() {
                "Select a file from the right panel to open its diff or file view."
            } else if ws.diff_task.is_some() {
                "Computing diff for the selected commit…"
            } else {
                // Commit selected but neither ready nor computing —
                // likely an error state the last_error banner already
                // explains. Keep the row present so height stays the
                // same; leave it empty.
                ""
            };
            ui.weak(msg);
            ui.separator();
        }

        // ---------- graph (includes Working Tree as virtual row) ----------
        let head_oid = ws.repo.head_oid();
        // Banner: graph was capped at MAX_GRAPH_COMMITS or lanes exceed
        // the visible width. Both are "we didn't render the whole story"
        // states that the user deserves to see — otherwise the graph
        // silently lies about the shape of their history.
        if let Some(gv) = &ws.graph_view {
            let g = &gv.graph;
            if g.truncated {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("⚠").color(egui::Color32::from_rgb(220, 170, 60)));
                    ui.weak(format!(
                        "Showing {} most-recent commits. This repo's history is larger; \
                         older commits aren't in the graph yet.",
                        g.rows.len()
                    ));
                });
            }
            if g.max_lane > crate::git::graph::MAX_GRAPH_LANES {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("⚠").color(egui::Color32::from_rgb(220, 170, 60)));
                    ui.weak(format!(
                        "{} concurrent branch lanes folded into {} visible columns \
                         (overflow rendered in the rightmost lane).",
                        g.max_lane + 1,
                        crate::git::graph::MAX_GRAPH_LANES + 1,
                    ));
                });
            }
            if g.truncated || g.max_lane > crate::git::graph::MAX_GRAPH_LANES {
                ui.separator();
            }
        }
        let mut clear_commit_selection = false;
        let mut clicked_commit_oid: Option<gix::ObjectId> = None;
        let mut basket_toggle_oid: Option<gix::ObjectId> = None;
        if let Some(gv) = &mut ws.graph_view {
            // Get working tree entries for the virtual row
            let working_entries = ws.repo_ui_cache.as_ref().and_then(|c| c.working.as_deref());

            let result = gv.show(
                ui,
                head_oid,
                &mut ws.column_prefs,
                working_entries,
                &mut ws.selected_working_tree,
                &mut ws.working_tree_expanded,
                &ws.commit_basket,
            );
            if let Some(action) = result.action {
                intent.action = Some(action);
            }
            if result.open_commit {
                intent.open_commit = true;
            }
            clear_commit_selection = result.clear_commit_selection;
            if let Some(idx) = result.clicked {
                clicked_commit_oid = gv.graph.rows.get(idx).map(|row| row.oid);
            }
            if let Some(idx) = result.toggle_basket {
                basket_toggle_oid = gv.graph.rows.get(idx).map(|row| row.oid);
            }
        } else {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.weak("Graph unavailable (empty repo or build error).");
            });
        }
        if clear_commit_selection {
            ws.selected_commit = None;
            ws.current_diff = None;
            ws.selected_file_idx = None;
            ws.selected_file_view = SelectedFileView::Diff;
            ws.set_image_cache(None);
            ws.selected_working_file = None;
            ws.working_file_diff = None;
            ws.working_tree_expanded = false;
        }
        if let Some(oid) = clicked_commit_oid {
            // Click on real commit row clears working tree selection
            ws.selected_working_tree = false;
            ws.selected_working_file = None;
            ws.working_file_diff = None;
            ws.working_tree_expanded = false;
            commit_clicked = Some(oid);
        }
        if let Some(oid) = basket_toggle_oid {
            // Cmd/Ctrl-click toggled basket membership. Does NOT touch
            // `selected_commit` / `current_diff` — the user can still
            // be viewing one commit's diff while building a basket.
            if !ws.commit_basket.insert(oid) {
                ws.commit_basket.remove(&oid);
            }
        }
    });

    // After the closure releases the `&mut ws` borrow, kick off an async
    // diff computation for the clicked commit.
    //
    // Rationale: `diff_for_commit` shells out to `git show --raw --patch` for
    // rename detection, which is O(files²) in the worst case. On Linux-
    // kernel merge commits that touch 5000+ files this ran for several
    // seconds on the UI thread, freezing the window on every click.
    // Instead we spawn a worker, show "Computing diff…" in the right
    // pane, and replace the result in place when it lands.
    if let Some(oid) = commit_clicked {
        if let View::Workspace(tabs) = &mut app.view {
            let ws = tabs.current_mut();

            // 1. Always update the selection / clear per-file state so the
            //    row highlight follows the click instantly.
            let changed_commit = ws.selected_commit != Some(oid);
            ws.selected_commit = Some(oid);
            if changed_commit {
                ws.selected_file_idx = None;
                ws.selected_file_view = SelectedFileView::Diff;
                ws.set_image_cache(None);
            }

            // 2. Fast path — diff is already loaded or cached?
            let already_live = ws.current_diff.as_ref().is_some_and(|d| {
                // `current_diff` doesn't carry the oid it was built for; we
                // treat "selected_commit hasn't moved" as the cache key.
                // Combined with the cache map below, this means flipping
                // between two commits is a pure memcpy on the second visit.
                !changed_commit
            });
            if already_live {
                // Already showing the right diff — nothing to do.
            } else if let Some(cached) = ws.diff_cache.get(&oid) {
                // Cache hit: install instantly, no thread / no subprocess.
                ws.current_diff = Some(cached);
                // Cancel any in-flight stale request; the cached value
                // is newer than anything a worker might still return.
                ws.diff_task = None;
                ws.pending_diff_oid = None;
            } else if ws.diff_task.is_some() {
                // A worker is already running for some OTHER commit.
                // Queue this oid as "the one to compute next"; if the
                // user keeps clicking, later clicks just overwrite this
                // field, so we only ever chase the latest selection
                // instead of spawning a thread per click.
                ws.pending_diff_oid = Some(oid);
                // Clear the live diff so the UI shows "Computing…" for
                // the new target instead of stale content.
                ws.current_diff = None;
            } else {
                // Nothing running, nothing cached → spawn a worker now.
                ws.current_diff = None;
                spawn_diff_worker(ws, oid, ctx);
            }
        }
    }

    // Apply intents after the UI closure releases its borrow.
    if intent.undo {
        app.undo();
    }
    if intent.redo {
        app.redo();
    }
    if intent.open_panic {
        app.open_panic_recovery();
    }
    if intent.open_commit {
        app.commit_modal_open = true;
    }
    if intent.open_rebase {
        app.open_rebase_modal_for_head();
    }
    if intent.open_columns {
        app.columns_popover_open = true;
    }
    if intent.open_activity_log {
        app.activity_log_open = true;
    }
    if let Some(action) = intent.action {
        dispatch(app, Some(ctx), action);
    }
    if let Some(scope) = intent.new_scope {
        app.rebuild_graph(scope);
    }
}

/// Result of running a dispatched action — collected so we can release
/// the `&mut View::Workspace` borrow before mutating app-wide state.
#[derive(Default)]
struct DispatchOutcome {
    rebuild: Option<GraphScope>,
    hud: Option<String>,
    error: Option<String>,
    journal_entry: Option<(Operation, journal::RepoSnapshot, journal::RepoSnapshot)>,
    prompt: Option<PendingPrompt>,
    copy_text: Option<String>,
}

/// Execute a commit action coming from the graph's context menu.
/// Launch the background diff worker for `oid`. Stores the `DiffTask` on
/// `ws`. The worker wakes the main thread via `ctx.request_repaint()`
/// the moment its result is ready so `poll_diff_tasks` picks it up on
/// the very next frame without us having to spin a 60 Hz poll timer.
pub(crate) fn spawn_diff_worker(
    ws: &mut crate::app::WorkspaceState,
    oid: gix::ObjectId,
    ctx: &egui::Context,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    let repo_path = ws.repo.path().to_path_buf();
    let ctx_clone = ctx.clone();
    std::thread::spawn(move || {
        let result = crate::git::diff_for_commit(&repo_path, oid).map_err(|e| format!("{e:#}"));
        let _ = tx.send(result);
        ctx_clone.request_repaint();
    });
    ws.diff_task = Some(crate::app::DiffTask {
        oid,
        started_at: std::time::Instant::now(),
        rx,
    });
}

fn dispatch(app: &mut MergeFoxApp, ctx: Option<&egui::Context>, action: CommitAction) {
    let outcome = run_action(app, action);
    apply_outcome(app, ctx, outcome);
}

/// Public entry point for non-graph UIs (sidebar, top bar, stash menu)
/// that need to dispatch a `CommitAction`. No clipboard context is
/// threaded through — actions that require one are dispatched by the
/// graph handler instead.
pub fn dispatch_action(app: &mut MergeFoxApp, action: CommitAction) {
    dispatch(app, None, action);
}

fn clear_checkout_selection(ws: &mut crate::app::WorkspaceState) {
    ws.selected_commit = None;
    ws.current_diff = None;
    ws.selected_file_idx = None;
    ws.selected_file_view = SelectedFileView::Diff;
    ws.set_image_cache(None);
    if let Some(graph_view) = &mut ws.graph_view {
        graph_view.selected_row = None;
    }
}

fn branch_checkout_needs_rebuild(scope: GraphScope) -> bool {
    matches!(scope, GraphScope::CurrentBranch)
}

/// Second entry-point: called from `ui::prompt` when a pending modal is
/// confirmed. Converts the prompt back into a concrete git op.
pub fn dispatch_prompt(app: &mut MergeFoxApp, prompt: PendingPrompt) {
    let outcome = run_prompt(app, prompt);
    apply_outcome(app, None, outcome);
}

fn apply_outcome(app: &mut MergeFoxApp, ctx: Option<&egui::Context>, outcome: DispatchOutcome) {
    if let Some((op, before, after)) = outcome.journal_entry {
        app.journal_record(op, before, after);
    }
    if let Some(text) = outcome.copy_text {
        if let Some(ctx) = ctx {
            ctx.copy_text(text);
        }
    }
    if let Some(m) = outcome.hud {
        app.hud = Some(crate::app::Hud::new(m, 1800));
    }
    if let Some(e) = outcome.error {
        app.last_error = Some(e);
    }
    if let Some(p) = outcome.prompt {
        app.pending_prompt = Some(p);
    }
    if let Some(scope) = outcome.rebuild {
        app.rebuild_graph(scope);
    }
}

fn run_action(app: &mut MergeFoxApp, action: CommitAction) -> DispatchOutcome {
    let mut out = DispatchOutcome::default();
    let repo_prefs = match &app.view {
        View::Workspace(tabs) => app.config.repo_settings_for(tabs.current().repo.path()),
        View::Welcome(_) | View::OpeningRepo(_) => Default::default(),
    };

    let View::Workspace(tabs) = &mut app.view else {
        return out;
    };
    let ws = tabs.current_mut();
    let scope = ws.graph_scope;

    match action {
        // ---- read-only ----
        CommitAction::CopySha(oid) => {
            let s = oid.to_string();
            out.copy_text = Some(s.clone());
            out.hud = Some(format!("Copied SHA: {s}"));
        }
        CommitAction::CopyShortSha(oid) => {
            let s = oid.to_string();
            let short = s[..7.min(s.len())].to_string();
            out.copy_text = Some(short.clone());
            out.hud = Some(format!("Copied: {short}"));
        }

        // ---- navigation ----
        CommitAction::Checkout(oid) => {
            let before = journal::capture(ws.repo.path()).ok();
            // Auto-stash dirty changes so checkout never fails with conflicts
            // against local edits. This matches the undo/redo flow, so that
            // round-tripping through checkouts + undo stays consistent.
            let stashed = ws
                .repo
                .auto_stash_if_dirty(&format!("checkout {}", short_sha(&oid)));
            if let Err(e) = stashed {
                out.error = Some(format!("auto-stash: {e:#}"));
                return out;
            }
            match ws.repo.checkout_commit(oid) {
                Ok(()) => {
                    if let (Some(b), Ok(a)) = (before, journal::capture(ws.repo.path())) {
                        out.journal_entry = Some((
                            Operation::Checkout {
                                from: b.head_branch.clone(),
                                to: format!("detached@{}", short_sha(&oid)),
                            },
                            b,
                            a,
                        ));
                    }
                    let stash_suffix = if matches!(stashed, Ok(true)) {
                        " (stashed dirty changes)"
                    } else {
                        ""
                    };
                    out.hud = Some(format!(
                        "Checked out {} (detached){stash_suffix}",
                        short_sha(&oid)
                    ));
                    clear_checkout_selection(ws);
                }
                Err(e) => out.error = Some(format!("checkout {}: {e:#}", short_sha(&oid))),
            }
        }
        CommitAction::CheckoutBranch(name) => {
            let before = journal::capture(ws.repo.path()).ok();
            let stashed = ws.repo.auto_stash_if_dirty(&format!("checkout {name}"));
            if let Err(e) = stashed {
                out.error = Some(format!("auto-stash: {e:#}"));
                return out;
            }
            match ws.repo.checkout_branch(&name) {
                Ok(()) => {
                    if let (Some(b), Ok(a)) = (before, journal::capture(ws.repo.path())) {
                        out.journal_entry = Some((
                            Operation::Checkout {
                                from: b.head_branch.clone(),
                                to: name.clone(),
                            },
                            b,
                            a,
                        ));
                    }
                    let stash_suffix = if matches!(stashed, Ok(true)) {
                        " (stashed dirty changes)"
                    } else {
                        ""
                    };
                    out.hud = Some(format!("Checked out {name}{stash_suffix}"));
                    clear_checkout_selection(ws);
                    if branch_checkout_needs_rebuild(scope) {
                        out.rebuild = Some(scope);
                    }
                }
                Err(e) => out.error = Some(format!("checkout {name}: {e:#}")),
            }
        }

        // ---- revert / cherry-pick ----
        CommitAction::Revert(oid) => {
            let before = journal::capture(ws.repo.path()).ok();
            match ws.repo.revert_commit(oid) {
                Ok(new_oid) => {
                    if let (Some(b), Ok(a)) = (before, journal::capture(ws.repo.path())) {
                        out.journal_entry = Some((
                            Operation::Revert {
                                commits: vec![oid.to_string()],
                            },
                            b,
                            a,
                        ));
                    }
                    out.hud = Some(format!(
                        "Reverted {} → {}",
                        short_sha(&oid),
                        short_sha(&new_oid)
                    ));
                    out.rebuild = Some(scope);
                }
                Err(e) => {
                    if !matches!(ws.repo.state(), crate::git::RepoState::Clean) {
                        out.hud = Some(format!(
                            "Resolve conflicts for {} to continue the revert",
                            short_sha(&oid)
                        ));
                    } else {
                        out.error = Some(format!("revert {}: {e:#}", short_sha(&oid)));
                    }
                }
            }
        }
        CommitAction::CherryPick(oids) => {
            if oids.is_empty() {
                return out;
            }
            let before = journal::capture(ws.repo.path()).ok();
            let mut applied: Vec<gix::ObjectId> = Vec::with_capacity(oids.len());
            let mut last_new: Option<gix::ObjectId> = None;
            let mut failure: Option<(gix::ObjectId, anyhow::Error)> = None;
            // Loop one commit at a time so we journal each pick and
            // surface conflicts at the exact commit that stopped the
            // sequence — the user needs to know "picks 1 and 2 landed,
            // pick 3 of 5 is mid-conflict".
            for oid in &oids {
                match ws.repo.cherry_pick_commit(*oid) {
                    Ok(new_oid) => {
                        applied.push(*oid);
                        last_new = Some(new_oid);
                    }
                    Err(e) => {
                        failure = Some((*oid, e));
                        break;
                    }
                }
            }
            if !applied.is_empty() {
                if let (Some(b), Ok(a)) = (before, journal::capture(ws.repo.path())) {
                    out.journal_entry = Some((
                        Operation::CherryPick {
                            commits: applied.iter().map(|o| o.to_string()).collect(),
                        },
                        b,
                        a,
                    ));
                }
                out.rebuild = Some(scope);
            }
            match failure {
                None => {
                    let summary = match (applied.len(), last_new) {
                        (1, Some(new_oid)) => format!(
                            "Cherry-picked {} → {}",
                            short_sha(&applied[0]),
                            short_sha(&new_oid)
                        ),
                        (n, _) => format!("Cherry-picked {n} commits"),
                    };
                    out.hud = Some(summary);
                }
                Some((failed_oid, e)) => {
                    let picked = applied.len();
                    let total = oids.len();
                    if !matches!(ws.repo.state(), crate::git::RepoState::Clean) {
                        out.hud = Some(format!(
                            "Picked {picked}/{total} — resolve conflicts for {} to continue",
                            short_sha(&failed_oid)
                        ));
                    } else {
                        out.error = Some(format!(
                            "cherry-pick {} ({picked}/{total} applied): {e:#}",
                            short_sha(&failed_oid)
                        ));
                    }
                }
            }
        }

        // ---- reset ----
        CommitAction::Reset {
            branch,
            mode,
            target,
        } => {
            // Hard reset opens a confirmation prompt first — caller only gets
            // to actually reset after confirming.
            if matches!(mode, ResetMode::Hard) {
                let preflight = Some(crate::preflight::hard_reset(
                    ws.repo.path(),
                    &branch,
                    target,
                ));
                out.prompt = Some(prompt::hard_reset_confirm(branch, target, preflight));
            } else {
                let before = journal::capture(ws.repo.path()).ok();
                match ws.repo.reset(mode, target) {
                    Ok(()) => {
                        if let (Some(b), Ok(a)) = (before, journal::capture(ws.repo.path())) {
                            out.journal_entry = Some((
                                Operation::Reset {
                                    branch: branch.clone(),
                                    mode: format!("{mode:?}").to_lowercase(),
                                    target: target.to_string(),
                                },
                                b,
                                a,
                            ));
                        }
                        out.hud = Some(format!(
                            "Reset {branch} [{mode:?}] → {}",
                            short_sha(&target)
                        ));
                        out.rebuild = Some(scope);
                    }
                    Err(e) => {
                        out.error = Some(format!("reset {branch}: {e:#}"));
                    }
                }
            }
        }
        // ---- prompts (collect input first, run on confirm) ----
        CommitAction::CreateBranchPrompt(oid) => {
            out.prompt = Some(prompt::create_branch_prompt(oid));
        }
        CommitAction::CreateTagPrompt { at, annotated } => {
            out.prompt = Some(prompt::create_tag_prompt(at, annotated));
        }
        CommitAction::RenameBranchPrompt { from } => {
            out.prompt = Some(prompt::rename_branch_prompt(from));
        }
        CommitAction::DeleteBranchPrompt { name, is_remote } => {
            let preflight = Some(crate::preflight::delete_branch(
                ws.repo.path(),
                &name,
                is_remote,
            ));
            out.prompt = Some(prompt::delete_branch_confirm(name, is_remote, preflight));
        }
        CommitAction::SetUpstreamPrompt { branch } => {
            // Prefer the cached snapshot, but if it is empty/stale
            // re-read the configured remotes once when the user opens
            // the prompt so existing upstreams don't collapse into the
            // "add a remote first" empty state.
            let mut remotes = ws
                .repo_ui_cache
                .as_ref()
                .map(|c| c.remotes.clone())
                .unwrap_or_default();
            if remotes.is_empty() {
                remotes = ws
                    .repo
                    .list_remotes()
                    .ok()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|remote| remote.fetch_url.is_some() || remote.push_url.is_some())
                    .map(|remote| remote.name)
                    .collect();
            }
            let current_upstream = ws.repo_ui_cache.as_ref().and_then(|c| {
                c.branches
                    .iter()
                    .find(|b| !b.is_remote && b.name == branch)
                    .and_then(|b| b.upstream.clone())
            });
            let remote_branches = ws
                .repo_ui_cache
                .as_ref()
                .map(|c| {
                    c.branches
                        .iter()
                        .filter(|b| b.is_remote)
                        .map(|b| b.name.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            out.prompt = Some(prompt::set_upstream_prompt(
                branch,
                remotes,
                current_upstream,
                remote_branches,
            ));
        }
        CommitAction::AmendMessagePrompt => {
            let current_author = match crate::git::ops::head_commit_author(ws.repo.path()) {
                Ok(author) => author,
                Err(_) => {
                    out.error = Some("no commits to amend yet".to_string());
                    return out;
                }
            };
            let current = crate::git::cli::run(ws.repo.path(), ["log", "-1", "--format=%B"])
                .map(|out| out.stdout_str().trim_end_matches('\n').to_string())
                .unwrap_or_default();
            // Pre-flight: is the HEAD commit already on a remote?
            // If so the modal surfaces a "this will need force-push"
            // warning inline before the user commits.
            let preflight = Some(crate::preflight::amend_head(ws.repo.path()));
            out.prompt = Some(prompt::amend_message_prompt(
                current,
                Some(current_author),
                preflight,
            ));
        }

        // ---- background network ops ----
        CommitAction::Pull { branch } => {
            let strategy = repo_prefs.pull_strategy.to_git();
            if let Some((remote, upstream_branch)) = tracked_upstream_for_branch(ws, &branch) {
                app.start_pull(&remote, &upstream_branch, strategy);
            } else {
                out.error = Some(format!("no upstream configured for `{branch}`"));
            }
            return out;
        }
        CommitAction::Push { branch, force } => {
            let remote = default_remote(ws, &app.config);
            app.start_push(&remote, &branch, force);
            return out;
        }
        CommitAction::PushTag { tag } => {
            let remote = default_remote(ws, &app.config);
            app.start_push_tag(&remote, &tag);
            return out;
        }
        CommitAction::PushAllTags => {
            let remote = default_remote(ws, &app.config);
            // No confirmation for now — `--tags` is mostly additive
            // (git won't delete remote tags, only upload missing
            // local ones). If we ever expose `--follow-tags` with
            // deletion semantics, it needs a preflight modal.
            app.start_push_all_tags(&remote);
            return out;
        }

        // ---- not yet implemented ----
        CommitAction::DropCommitPrompt(oid)
        | CommitAction::MoveCommitUp(oid)
        | CommitAction::MoveCommitDown(oid)
        | CommitAction::CreateWorktreePrompt(oid) => {
            out.error = Some(format!(
                "'{}' isn't wired up yet — tracked for the rebase / worktree milestone.",
                describe_pending(&oid)
            ));
        }

        // ---- stash ----
        CommitAction::StashPushPrompt => {
            out.prompt = Some(prompt::stash_push_prompt());
        }
        CommitAction::StashPop { index } => {
            let before = journal::capture(ws.repo.path()).ok();
            // Capture the stash's own oid/message before popping so the
            // journal entry records which stash was applied.
            let stash_oid = ws
                .repo_ui_cache
                .as_ref()
                .and_then(|c| c.stashes.as_ref())
                .and_then(|list| list.iter().find(|s| s.index == index))
                .map(|s| s.oid.to_string())
                .unwrap_or_default();
            match crate::git::ops::stash_pop(ws.repo.path(), index) {
                Ok(()) => {
                    if let (Some(b), Ok(a)) = (before, journal::capture(ws.repo.path())) {
                        out.journal_entry = Some((Operation::StashPop { stash_oid }, b, a));
                    }
                    out.hud = Some(format!("Popped stash@{{{index}}}"));
                    out.rebuild = Some(scope);
                }
                Err(e) => out.error = Some(format!("stash pop: {e:#}")),
            }
        }
        CommitAction::StashApply { index } => {
            let before = journal::capture(ws.repo.path()).ok();
            let refspec = format!("stash@{{{index}}}");
            match crate::git::cli::run(ws.repo.path(), ["stash", "apply", &refspec]) {
                Ok(_) => {
                    if let (Some(b), Ok(a)) = (before, journal::capture(ws.repo.path())) {
                        out.journal_entry = Some((
                            Operation::Raw {
                                label: format!("Apply stash@{{{index}}}"),
                            },
                            b,
                            a,
                        ));
                    }
                    out.hud = Some(format!("Applied stash@{{{index}}} (not dropped)"));
                    out.rebuild = Some(scope);
                }
                Err(e) => out.error = Some(format!("stash apply: {e:#}")),
            }
        }
        CommitAction::StashDropPrompt { index, message } => {
            out.prompt = Some(prompt::drop_stash_confirm(index, message));
        }
    }

    out
}

fn run_prompt(app: &mut MergeFoxApp, prompt: PendingPrompt) -> DispatchOutcome {
    let mut out = DispatchOutcome::default();

    let View::Workspace(tabs) = &mut app.view else {
        return out;
    };
    let ws = tabs.current_mut();
    let scope = ws.graph_scope;

    match prompt {
        PendingPrompt::CreateBranch { at, name, .. } => {
            let name = name.trim().to_string();
            if name.is_empty() {
                return out;
            }
            let before = journal::capture(ws.repo.path()).ok();
            match ws.repo.create_branch(&name, at) {
                Ok(()) => {
                    if let (Some(b), Ok(a)) = (before, journal::capture(ws.repo.path())) {
                        out.journal_entry = Some((
                            Operation::CreateBranch {
                                name: name.clone(),
                                at: at.to_string(),
                            },
                            b,
                            a,
                        ));
                    }
                    out.hud = Some(format!("Created branch {name} at {}", short_sha(&at)));
                    out.rebuild = Some(scope);
                }
                Err(e) => out.error = Some(format!("create branch: {e:#}")),
            }
        }
        PendingPrompt::RenameBranch { from, to, .. } => {
            let to = to.trim().to_string();
            if to.is_empty() || to == from {
                return out;
            }
            let before = journal::capture(ws.repo.path()).ok();
            match ws.repo.rename_branch(&from, &to) {
                Ok(()) => {
                    if let (Some(b), Ok(a)) = (before, journal::capture(ws.repo.path())) {
                        // Rename = delete-old + create-new; we log it as Raw
                        // until we add a proper Rename variant.
                        out.journal_entry = Some((
                            Operation::Raw {
                                label: format!("Rename branch {from} → {to}"),
                            },
                            b,
                            a,
                        ));
                    }
                    out.hud = Some(format!("Renamed {from} → {to}"));
                    out.rebuild = Some(scope);
                }
                Err(e) => out.error = Some(format!("rename branch: {e:#}")),
            }
        }
        PendingPrompt::CreateTag {
            at,
            name,
            message,
            annotated,
            ..
        } => {
            let name = name.trim().to_string();
            if name.is_empty() {
                return out;
            }
            let msg = if annotated {
                Some(message.as_str())
            } else {
                None
            };
            let before = journal::capture(ws.repo.path()).ok();
            match ws.repo.create_tag(&name, at, msg) {
                Ok(_) => {
                    if let (Some(b), Ok(a)) = (before, journal::capture(ws.repo.path())) {
                        out.journal_entry = Some((
                            Operation::Raw {
                                label: format!("Tag {name} → {}", short_sha(&at)),
                            },
                            b,
                            a,
                        ));
                    }
                    out.hud = Some(format!("Tagged {name}"));
                    out.rebuild = Some(scope);
                }
                Err(e) => out.error = Some(format!("create tag: {e:#}")),
            }
        }
        PendingPrompt::SetUpstream {
            branch,
            selected_remote,
            remote_branch,
            new_remote,
            ..
        } => {
            // If the user filled in the "add new remote" form inline, create
            // the remote FIRST (so upstream tracking can actually resolve),
            // then fall through to the set-upstream step pointing at the
            // newly-created remote. This is the "publish a brand-new repo"
            // happy path.
            let effective_remote = if let Some(draft) = new_remote {
                let name = draft.name.trim().to_string();
                let fetch_url = draft.fetch_url.trim().to_string();
                let push_url = draft.push_url.trim().to_string();
                let push_opt = if push_url.is_empty() {
                    None
                } else {
                    Some(push_url.as_str())
                };
                if name.is_empty() || fetch_url.is_empty() {
                    out.error = Some("Remote name and fetch URL are required".to_string());
                    return out;
                }
                if let Err(e) = ws.repo.add_remote(&name, &fetch_url, push_opt) {
                    out.error = Some(format!("add remote `{name}`: {e:#}"));
                    return out;
                }
                Some(name)
            } else {
                selected_remote
            };

            // With the remote settled, wire the upstream.
            let ref_path = effective_remote.as_ref().map(|r| {
                let rb = remote_branch.trim();
                let rb = if rb.is_empty() { branch.as_str() } else { rb };
                format!("{r}/{rb}")
            });
            let value = ref_path.as_deref();
            match ws.repo.set_upstream(&branch, value) {
                Ok(()) => {
                    out.hud = Some(match value {
                        Some(u) => format!("Set upstream {branch} → {u}"),
                        None => format!("Cleared upstream for {branch}"),
                    });
                    out.rebuild = Some(scope);
                }
                Err(e) => out.error = Some(format!("set upstream: {e:#}")),
            }
        }
        PendingPrompt::AmendMessage {
            message,
            current_author,
            author_override,
            author_name,
            author_email,
            ..
        } => {
            let message = message.trim().to_string();
            if message.is_empty() {
                return out;
            }
            let author = if author_override {
                match crate::git::ops::CommitAuthor::normalized(&author_name, &author_email) {
                    Ok(author) if current_author.as_ref() == Some(&author) => None,
                    Ok(author) => Some(author),
                    Err(e) => {
                        out.error = Some(format!("amend author: {e:#}"));
                        return out;
                    }
                }
            } else {
                None
            };
            let before = journal::capture(ws.repo.path()).ok();
            match crate::git::ops::amend(ws.repo.path(), Some(&message), author.as_ref()) {
                Ok(_) => {
                    if let (Some(b), Ok(a)) = (before, journal::capture(ws.repo.path())) {
                        out.journal_entry = Some((
                            Operation::Commit {
                                message: message.clone(),
                                amended: true,
                            },
                            b,
                            a,
                        ));
                    }
                    out.hud = Some("Amended HEAD".to_string());
                    out.rebuild = Some(scope);
                }
                Err(e) => out.error = Some(format!("amend: {e:#}")),
            }
        }
        PendingPrompt::StashPush { message, .. } => {
            // Empty message lets git fall back to its default
            // "WIP on <branch>: <sha> <subject>".
            let msg = message.trim();
            let stash_msg = if msg.is_empty() {
                // `git stash push` refuses -m with an empty string; for the
                // default message we just omit the flag via the CLI helper.
                None
            } else {
                Some(msg.to_string())
            };
            let before = journal::capture(ws.repo.path()).ok();
            let result = match &stash_msg {
                Some(m) => crate::git::ops::stash_push(ws.repo.path(), m).map(Some),
                None => crate::git::cli::run(ws.repo.path(), ["stash", "push", "-u"]).map(|_| None),
            };
            match result {
                Ok(_) => {
                    if let (Some(b), Ok(a)) = (before, journal::capture(ws.repo.path())) {
                        out.journal_entry = Some((
                            Operation::StashPush {
                                message: stash_msg.clone().unwrap_or_else(|| "WIP".to_string()),
                            },
                            b,
                            a,
                        ));
                    }
                    out.hud = Some("Stashed working tree".to_string());
                    out.rebuild = Some(scope);
                }
                Err(e) => {
                    // Most common: "No local changes to save".
                    let text = format!("{e:#}");
                    if text.contains("No local changes to save") {
                        out.hud = Some("Nothing to stash — working tree clean".into());
                    } else {
                        out.error = Some(format!("stash push: {text}"));
                    }
                }
            }
        }
        PendingPrompt::Confirm { kind, .. } => match kind {
            crate::ui::prompt::ConfirmKind::DeleteBranch {
                name,
                is_remote,
                force,
            } => {
                let before = journal::capture(ws.repo.path()).ok();
                let tip = ws.repo.tip_of(&name, is_remote).ok();
                match ws.repo.delete_branch(&name, is_remote, force) {
                    Ok(()) => {
                        if let (Some(b), Ok(a)) = (before, journal::capture(ws.repo.path())) {
                            out.journal_entry = Some((
                                Operation::DeleteBranch {
                                    name: name.clone(),
                                    tip: tip.map(|o| o.to_string()).unwrap_or_default(),
                                },
                                b,
                                a,
                            ));
                        }
                        out.hud = Some(if force {
                            format!("Force-deleted branch {name}")
                        } else {
                            format!("Deleted branch {name}")
                        });
                        out.rebuild = Some(scope);
                    }
                    Err(e) => {
                        out.error = Some(format_delete_branch_error(&name, force, &e));
                    }
                }
            }
            crate::ui::prompt::ConfirmKind::HardReset { branch, target } => {
                let before = journal::capture(ws.repo.path()).ok();
                match ws.repo.reset(ResetMode::Hard, target) {
                    Ok(()) => {
                        if let (Some(b), Ok(a)) = (before, journal::capture(ws.repo.path())) {
                            out.journal_entry = Some((
                                Operation::Reset {
                                    branch: branch.clone(),
                                    mode: "hard".into(),
                                    target: target.to_string(),
                                },
                                b,
                                a,
                            ));
                        }
                        out.hud = Some(format!("Hard-reset {branch} → {}", short_sha(&target)));
                        out.rebuild = Some(scope);
                    }
                    Err(e) => out.error = Some(format!("hard reset: {e:#}")),
                }
            }
            crate::ui::prompt::ConfirmKind::DropCommit { oid } => {
                out.error = Some(format!(
                    "Drop commit {} not yet wired up — coming with the rebase milestone.",
                    short_sha(&oid)
                ));
            }
            crate::ui::prompt::ConfirmKind::ForcePush { remote, branch } => {
                app.start_push(&remote, &branch, true);
                return out;
            }
            crate::ui::prompt::ConfirmKind::DropStash { index, message: _ } => {
                let before = journal::capture(ws.repo.path()).ok();
                let refspec = format!("stash@{{{index}}}");
                match crate::git::cli::run(ws.repo.path(), ["stash", "drop", &refspec]) {
                    Ok(_) => {
                        if let (Some(b), Ok(a)) = (before, journal::capture(ws.repo.path())) {
                            out.journal_entry = Some((
                                Operation::Raw {
                                    label: format!("Drop stash@{{{index}}}"),
                                },
                                b,
                                a,
                            ));
                        }
                        out.hud = Some(format!("Dropped stash@{{{index}}}"));
                        out.rebuild = Some(scope);
                    }
                    Err(e) => out.error = Some(format!("stash drop: {e:#}")),
                }
            }
            crate::ui::prompt::ConfirmKind::DiscardHunk {
                file,
                hunk_index,
                line_indices,
            } => {
                // Re-fetch the unstaged diff now (not at prompt-open
                // time) so concurrent editor saves can't fool the
                // patch with a stale view. `git apply --check` inside
                // `discard_hunk` will catch residual mismatches and
                // surface them via the error path.
                let repo_path = ws.repo.path().to_path_buf();
                let entry = ws
                    .repo_ui_cache
                    .as_ref()
                    .and_then(|c| c.working.as_ref())
                    .and_then(|entries| entries.iter().find(|e| e.path == file).cloned());
                match entry {
                    Some(e) => {
                        let side_text = crate::git::diff_text_unstaged_only(&repo_path, &e)
                            .unwrap_or_default();
                        let fd = crate::git::file_diff_for_working_entry(&e, &side_text);
                        let sel = crate::git::hunk_staging::HunkSelector {
                            file: file.clone(),
                            hunk_index,
                            line_indices,
                        };
                        match crate::git::hunk_staging::discard_hunk(&repo_path, &fd, &sel) {
                            Ok(()) => {
                                out.hud = Some(format!(
                                    "Discarded hunk {} in {}",
                                    hunk_index + 1,
                                    file.display()
                                ));
                                ws.working_file_diff = None;
                                ws.hunk_selection.selected_lines.clear();
                            }
                            Err(err) => {
                                out.error = Some(format!("discard hunk: {err:#}"));
                            }
                        }
                    }
                    None => {
                        out.error =
                            Some("discard hunk: file no longer in working tree status".into());
                    }
                }
            }
        },
    }

    out
}

fn default_remote(ws: &crate::app::WorkspaceState, config: &Config) -> String {
    default_remote_name(ws, config)
}

fn short_sha(oid: &gix::ObjectId) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}

fn describe_pending(oid: &gix::ObjectId) -> String {
    format!("op on {}", short_sha(oid))
}

/// Render the expanded working tree file list with staged/unstaged sections.
fn render_working_tree_files(
    ui: &mut egui::Ui,
    ws: &mut crate::app::WorkspaceState,
    entries: &[crate::git::StatusEntry],
    intent: &mut PanelIntent,
) {
    use crate::git::EntryKind;

    let staged: Vec<&crate::git::StatusEntry> = entries.iter().filter(|e| e.staged).collect();
    let unstaged: Vec<&crate::git::StatusEntry> = entries
        .iter()
        .filter(|e| e.unstaged || matches!(e.kind, EntryKind::Untracked) || e.conflicted)
        .collect();

    // Staged section
    if !staged.is_empty() {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("📦 Staged").strong().small());
            ui.weak(format!("({})", staged.len()));
        });
        ui.indent("staged_indent", |ui| {
            for entry in &staged {
                render_working_file_row(ui, ws, entry, true, intent);
            }
        });
        ui.add_space(4.0);
    }

    // Unstaged section
    if !unstaged.is_empty() {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("🗂 Unstaged").strong().small());
            ui.weak(format!("({})", unstaged.len()));
        });
        ui.indent("unstaged_indent", |ui| {
            for entry in &unstaged {
                render_working_file_row(ui, ws, entry, false, intent);
            }
        });
        ui.add_space(4.0);
    }

    // Action buttons at the bottom of expanded section
    ui.horizontal(|ui| {
        if ui.button("Stage all").clicked() {
            intent.action = Some(crate::actions::CommitAction::StashPushPrompt);
            // Actually we need a proper stage-all action, use open_commit for now
            intent.open_commit = true;
        }
        if ui.button("Open commit dialog…").clicked() {
            intent.open_commit = true;
        }
    });
}

/// Render a single file row in the working tree list.
fn render_working_file_row(
    ui: &mut egui::Ui,
    ws: &mut crate::app::WorkspaceState,
    entry: &crate::git::StatusEntry,
    is_staged_section: bool,
    _intent: &mut PanelIntent,
) {
    let (color, glyph) = style_for_working_entry(&entry.kind, entry.staged, entry.unstaged);

    let is_selected = ws
        .selected_working_file
        .as_ref()
        .map(|p| p == &entry.path)
        .unwrap_or(false);

    let row_height = 22.0;
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), row_height),
        egui::Sense::click(),
    );

    // Selection/hover background
    if is_selected {
        ui.painter().rect_filled(
            rect,
            2.0,
            ui.visuals().selection.bg_fill.gamma_multiply(0.4),
        );
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 2.0, ui.visuals().faint_bg_color.gamma_multiply(1.2));
    }

    // Content
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );

    // Status glyph
    child.label(egui::RichText::new(glyph).color(color).monospace().strong());
    child.add_space(4.0);

    // Path (truncated)
    let path_str = entry.path.display().to_string();
    child.add(
        egui::Label::new(egui::RichText::new(&path_str).monospace().small())
            .truncate()
            .selectable(false),
    );

    // Right side: status indicators
    let mut right_child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );

    if entry.conflicted {
        right_child.colored_label(egui::Color32::from_rgb(240, 90, 90), "⚠ conflicted");
    }
    if is_staged_section && entry.unstaged {
        right_child.weak(egui::RichText::new("+unstaged").small());
    }

    if resp.clicked() {
        if is_selected {
            ws.selected_working_file = None;
            ws.working_file_diff = None;
        } else {
            ws.selected_working_file = Some(entry.path.clone());
            ws.working_file_diff =
                crate::git::diff_text_for_working_entry(ws.repo.path(), entry).ok();
        }
    }

    // Tooltip with full path
    resp.on_hover_text(&path_str);
}

/// Style for working tree entries (similar to commit_modal).
fn style_for_working_entry(
    kind: &crate::git::EntryKind,
    staged: bool,
    _unstaged: bool,
) -> (egui::Color32, &'static str) {
    use crate::git::EntryKind;
    let base_color = match kind {
        EntryKind::New | EntryKind::Untracked => egui::Color32::from_rgb(90, 180, 120),
        EntryKind::Modified => egui::Color32::from_rgb(220, 190, 90),
        EntryKind::Deleted => egui::Color32::from_rgb(220, 100, 100),
        EntryKind::Renamed => egui::Color32::from_rgb(150, 150, 220),
        EntryKind::Typechange => egui::Color32::from_rgb(200, 120, 200),
        EntryKind::Conflicted => egui::Color32::from_rgb(255, 80, 80),
    };
    let color = if staged {
        base_color
    } else {
        egui::Color32::from_rgba_unmultiplied(base_color.r(), base_color.g(), base_color.b(), 160)
    };
    (color, kind.glyph())
}

fn format_delete_branch_error(name: &str, force: bool, err: &anyhow::Error) -> String {
    let raw = format!("{err:#}");
    let lower = raw.to_ascii_lowercase();
    if !force && lower.contains("not fully merged") {
        return format!(
            "delete branch: `{name}` is not fully merged, so safe delete was refused. Re-run the prompt and choose `Force delete` if you really want to remove it."
        );
    }
    format!("delete branch: {raw}")
}
