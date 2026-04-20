use std::path::PathBuf;

use crate::actions::CommitAction;
use crate::app::{MergeFoxApp, View};
use crate::git::{BranchInfo, LfsCandidate, LfsLock, LfsScanResult, StashEntry};
use crate::ui::profile_rules;

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    let sidebar_fill = crate::ui::theme::sidebar_fill(&app.config.theme);
    let language = app.config.ui_language;

    let (
        branch_error,
        stash_error,
        local,
        remote,
        configured_remotes,
        stashes,
        current_branch,
        head_branch,
        forge,
        lfs,
        locks,
    ) = {
        let View::Workspace(tabs) = &mut app.view else {
            return;
        };
        let ws = tabs.current_mut();

        // Branch list + stash list come from the shared per-tab cache
        // so we don't re-walk every ref + the stash reflog via gix
        // on each paint (that used to cost tens of ms per frame on a
        // moderately large repo, which showed up as click latency).
        // `MergeFoxApp::refresh_repo_ui_cache` keeps this fresh after
        // every op / tab switch.
        let (branch_error, stash_error, branches, stashes) = match &ws.repo_ui_cache {
            Some(cache) => (
                cache.branch_error.clone(),
                cache.stash_error.clone(),
                cache.branches.clone(),
                cache.stashes.clone(),
            ),
            None => (None, None, Vec::new(), None),
        };
        let configured_remotes = ws
            .repo_ui_cache
            .as_ref()
            .map(|cache| cache.remotes.clone())
            .unwrap_or_default();
        let head_branch = branches
            .iter()
            .find(|branch| branch.is_head && !branch.is_remote)
            .map(|branch| branch.name.clone());
        let (local, remote): (Vec<_>, Vec<_>) =
            branches.into_iter().partition(|branch| !branch.is_remote);

        // Snapshot the LFS state we need for rendering (running flag,
        // dismiss flag, scan result). We clone the result so we can drop
        // the workspace borrow before taking layout-related app calls.
        let lfs = LfsViewState {
            running: ws.lfs_scan.running.is_some(),
            dismissed: ws.lfs_scan.dismissed,
            result: ws.lfs_scan.result.clone(),
        };

        // Per-profile snapshot for the "File locks" panel. We only
        // populate this when the profile opts in, so General-profile
        // repos skip the clone entirely.
        let locks = {
            let rules = profile_rules::rules_for(ws.workspace_profile);
            if rules.show_lfs_lock_controls {
                Some(LfsLocksViewState {
                    locks: ws.lfs_locks.clone(),
                    unavailable_reason: ws.lfs_locks_unavailable_reason.clone(),
                    refreshing: ws.lfs_locks_refresh_task.is_some(),
                    current_user: ws.git_user_name.clone(),
                })
            } else {
                None
            }
        };

        (
            branch_error,
            stash_error,
            local,
            remote,
            configured_remotes,
            stashes,
            ws.selected_branch.clone(),
            head_branch,
            ws.forge.clone(),
            lfs,
            locks,
        )
    };

    let mut select_branch: Option<String> = None;
    let mut forge_action: Option<crate::ui::forge::SidebarAction> = None;
    let mut dismiss_lfs = false;
    let mut stash_action: Option<CommitAction> = None;
    let mut branch_action: Option<CommitAction> = None;
    let mut open_publish_remote: Option<String> = None;
    let mut lock_intent: Option<LockIntent> = None;

    egui::SidePanel::left("branches")
        .resizable(true)
        .default_width(260.0)
        .frame(egui::Frame::side_top_panel(ctx.style().as_ref()).fill(sidebar_fill))
        .show(ctx, |ui| {
            ui.heading("Branches");
            ui.separator();

            if let Some(err) = &branch_error {
                ui.colored_label(egui::Color32::LIGHT_RED, err);
                ui.add_space(6.0);
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.collapsing("Local", |ui| {
                    if local.is_empty() {
                        // Brand-new repo before first commit, or a freshly-
                        // inited workspace. Explain *why* and suggest the
                        // very next action so an empty panel doesn't look
                        // like a broken state.
                        ui.weak("No local branches yet.");
                        ui.weak("Create your first commit to bring up `main`.");
                        ui.add_space(4.0);
                    }
                    for branch in &local {
                        branch_row(
                            ui,
                            branch,
                            current_branch.as_deref(),
                            configured_remotes.is_empty()
                                && branch.is_head
                                && branch.upstream.is_none(),
                            &mut select_branch,
                            &mut branch_action,
                            &mut open_publish_remote,
                        );
                    }
                });
                ui.collapsing("Remote", |ui| {
                    if configured_remotes.is_empty() {
                        ui.weak("No remotes configured yet.");
                        ui.weak("Add one in Settings → Repository, or publish your current branch below.");
                        if let Some(branch) = head_branch.as_deref() {
                            if ui.small_button("Publish current branch…").clicked() {
                                open_publish_remote = Some(branch.to_string());
                            }
                        }
                        ui.add_space(4.0);
                    } else if remote.is_empty() {
                        ui.weak("No remote-tracking branches yet.");
                        ui.weak("Fetch from a configured remote to populate this list.");
                        ui.add_space(4.0);
                    }
                    for branch in &remote {
                        branch_row(
                            ui,
                            branch,
                            current_branch.as_deref(),
                            false,
                            &mut select_branch,
                            &mut branch_action,
                            &mut open_publish_remote,
                        );
                    }
                });
                // Stashes section with inline "Stash now" button in the header.
                // `CollapsingHeader::show_header` lets us add a button next to
                // the arrow without making the whole row the expand target.
                egui::CollapsingHeader::new("Stashes")
                    .default_open(true)
                    .show(ui, |ui| {
                        // "+ Stash" button sits above the list, inline with the
                        // stash count, so the affordance is always visible.
                        ui.horizontal(|ui| {
                            if ui
                                .small_button("+ Stash")
                                .on_hover_text(
                                    "Save working tree + index (incl. untracked) as a stash",
                                )
                                .clicked()
                            {
                                stash_action = Some(CommitAction::StashPushPrompt);
                            }
                            let count = stashes.as_ref().map(|s| s.len()).unwrap_or(0);
                            ui.weak(format!("({count})"));
                        });
                        ui.add_space(2.0);

                        if let Some(err) = &stash_error {
                            ui.colored_label(egui::Color32::from_rgb(230, 180, 90), err);
                            ui.add_space(4.0);
                        }

                        if let Some(stashes) = &stashes {
                            if stashes.is_empty() {
                                ui.weak("No stashes.");
                                ui.weak("Use + Stash above to save a work-in-progress snapshot.");
                            } else {
                                for stash in stashes {
                                    stash_row(ui, stash, &mut stash_action);
                                }
                            }
                        } else {
                            ui.weak("Unable to read stash list.");
                        }
                    });

                render_lfs_section(ui, &lfs, &mut dismiss_lfs);

                if let Some(locks) = locks.as_ref() {
                    render_lfs_locks_section(ui, locks, &mut lock_intent);
                }

                crate::ui::forge::show_sidebar(ui, language, &forge, &mut forge_action);
            });
        });

    if let Some(branch) = select_branch {
        if let View::Workspace(tabs) = &mut app.view {
            tabs.current_mut().selected_branch = Some(branch);
        }
    }

    if dismiss_lfs {
        if let View::Workspace(tabs) = &mut app.view {
            tabs.current_mut().lfs_scan.dismissed = true;
        }
    }

    if let Some(action) = stash_action {
        crate::ui::main_panel::dispatch_action(app, action);
    }
    if let Some(action) = branch_action {
        crate::ui::main_panel::dispatch_action(app, action);
    }
    if let Some(branch) = open_publish_remote {
        app.open_publish_remote_modal(Some(branch));
    }

    if let Some(action) = forge_action {
        match action {
            crate::ui::forge::SidebarAction::Refresh => app.refresh_active_forge(),
            crate::ui::forge::SidebarAction::NewPullRequest => app.open_pull_request_modal(),
            crate::ui::forge::SidebarAction::NewIssue => app.open_issue_modal(),
            crate::ui::forge::SidebarAction::Select(selection) => {
                if let View::Workspace(tabs) = &mut app.view {
                    tabs.current_mut().forge.selected = Some(selection);
                }
            }
        }
    }

    if let Some(intent) = lock_intent {
        dispatch_lock_intent(app, intent);
    }
}

/// What the "File locks" panel wants the outer handler to do.
/// Deliberately kept outside the render closure so the renderer can
/// stay pure (no `&mut MergeFoxApp`) — we collect an intent and
/// dispatch after the borrow ends, same pattern the rest of this
/// file uses for branch / stash actions.
pub(crate) enum LockIntent {
    Refresh,
    Unlock { path: PathBuf },
    ForceUnlockPrompt { path: PathBuf, owner: String },
}

fn dispatch_lock_intent(app: &mut MergeFoxApp, intent: LockIntent) {
    match intent {
        LockIntent::Refresh => app.refresh_active_lfs_locks(),
        LockIntent::Unlock { path } => app.start_lfs_unlock(path, false),
        LockIntent::ForceUnlockPrompt { path, owner } => {
            // Surface a confirm toast through the notification
            // center. We don't reuse the `PendingPrompt::Confirm`
            // machinery here because its `ConfirmKind` is already a
            // fairly long enum of destructive branch/stash actions —
            // dragging a cross-cutting "force unlock" variant into
            // it would widen several match arms in `prompt.rs`
            // needlessly. Instead we render our own tiny confirm
            // window right here in the sidebar via
            // `render_force_unlock_confirm` the next frame.
            app.force_unlock_confirm = Some(ForceUnlockConfirm { path, owner });
        }
    }
}

/// Confirm state for a pending "force unlock" click. Lives on
/// [`MergeFoxApp`] via [`MergeFoxApp::force_unlock_confirm`] so the
/// tiny modal survives frame-to-frame rerenders without going
/// through the full `PendingPrompt` machinery — force unlock isn't
/// destructive at the repo level, only on the lock server.
pub struct ForceUnlockConfirm {
    pub path: PathBuf,
    pub owner: String,
}

/// Hover text for the ahead/behind pill. Combined into one string so
/// both arrows share the same tooltip surface — otherwise users have
/// to hover twice to read both halves.
fn ahead_behind_tooltip(ahead: u32, behind: u32) -> String {
    match (ahead, behind) {
        (a, 0) => format!("{a} commit{} to push", if a == 1 { "" } else { "s" }),
        (0, b) => format!("{b} commit{} to pull", if b == 1 { "" } else { "s" }),
        (a, b) => format!(
            "Diverged: {a} local-only, {b} upstream-only commit{} — resolve with pull (merge or rebase)",
            if b == 1 { "" } else { "s" }
        ),
    }
}

fn branch_row(
    ui: &mut egui::Ui,
    branch: &BranchInfo,
    current_branch: Option<&str>,
    show_publish_button: bool,
    select_branch: &mut Option<String>,
    branch_action: &mut Option<CommitAction>,
    publish_branch: &mut Option<String>,
) {
    let selected = current_branch == Some(branch.name.as_str());
    let label = if branch.is_head {
        format!("● {}", branch.name)
    } else {
        format!("  {}", branch.name)
    };
    let resp = ui.horizontal(|ui| {
        let row = ui.selectable_label(selected, label);
        // Upstream indicator: shows either "→ origin/main" (good) or
        // "(no upstream)" in muted red (needs setup). Saves the user a
        // trip into Settings just to see whether push will work.
        if !branch.is_remote {
            match &branch.upstream {
                Some(u) => {
                    ui.weak(egui::RichText::new(format!("→ {u}")).small());
                    // Ahead/behind pill — shows the shape of the
                    // divergence at a glance. Hidden when both are zero
                    // so in-sync branches stay visually quiet. Color
                    // hints: orange for behind (need to pull), green-ish
                    // for ahead (ready to push).
                    if let (Some(ahead), Some(behind)) = (branch.ahead, branch.behind) {
                        if ahead > 0 || behind > 0 {
                            let mut parts: Vec<(String, egui::Color32)> = Vec::new();
                            if ahead > 0 {
                                parts.push((
                                    format!("↑{ahead}"),
                                    egui::Color32::from_rgb(116, 192, 136),
                                ));
                            }
                            if behind > 0 {
                                parts.push((
                                    format!("↓{behind}"),
                                    egui::Color32::from_rgb(220, 150, 80),
                                ));
                            }
                            for (text, color) in parts {
                                ui.label(
                                    egui::RichText::new(text).color(color).small().monospace(),
                                )
                                .on_hover_text(ahead_behind_tooltip(ahead, behind));
                            }
                        }
                    }
                }
                None => {
                    ui.weak(
                        egui::RichText::new("(no upstream)")
                            .small()
                            .color(egui::Color32::from_rgb(220, 150, 80)),
                    );
                    if show_publish_button && ui.small_button("Publish…").clicked() {
                        *publish_branch = Some(branch.name.clone());
                    }
                }
            }
        }
        row
    });
    let row = resp.inner;
    if row.clicked() {
        *select_branch = Some(branch.name.clone());
    }
    let tooltip = match (
        branch.last_commit_summary.as_deref(),
        branch.upstream.as_deref(),
    ) {
        (Some(summary), Some(u)) => format!("{summary}\n\nupstream: {u}"),
        (Some(summary), None) if !branch.is_remote => {
            format!("{summary}\n\nNo upstream configured. Right-click → Set upstream.")
        }
        (Some(summary), _) => summary.to_string(),
        (None, Some(u)) => format!("upstream: {u}"),
        (None, None) => String::new(),
    };
    let row = if tooltip.is_empty() {
        row
    } else {
        row.on_hover_text(tooltip)
    };

    // Context menu for local branches: the common branch ops that
    // previously only lived on the graph commit-row context menu.
    // Remote-tracking branches don't get write actions here.
    if !branch.is_remote {
        row.context_menu(|ui| {
            if show_publish_button && ui.button("Publish to new remote…").clicked() {
                *publish_branch = Some(branch.name.clone());
                ui.close_menu();
            }
            if ui.button("Set upstream…").clicked() {
                *branch_action = Some(CommitAction::SetUpstreamPrompt {
                    branch: branch.name.clone(),
                });
                ui.close_menu();
            }
            if ui
                .add_enabled(branch.upstream.is_some(), egui::Button::new("Pull"))
                .on_hover_text("Fetch and integrate the tracked remote branch")
                .clicked()
            {
                *branch_action = Some(CommitAction::Pull {
                    branch: branch.name.clone(),
                });
                ui.close_menu();
            }
            if ui.button("Push").clicked() {
                *branch_action = Some(CommitAction::Push {
                    branch: branch.name.clone(),
                    force: false,
                });
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Rename…").clicked() {
                *branch_action = Some(CommitAction::RenameBranchPrompt {
                    from: branch.name.clone(),
                });
                ui.close_menu();
            }
            if ui
                .button(egui::RichText::new("Delete…").color(egui::Color32::LIGHT_RED))
                .clicked()
            {
                *branch_action = Some(CommitAction::DeleteBranchPrompt {
                    name: branch.name.clone(),
                    is_remote: false,
                });
                ui.close_menu();
            }
        });
    }
}

fn stash_row(ui: &mut egui::Ui, stash: &StashEntry, action: &mut Option<CommitAction>) {
    let short = short_sha(&stash.oid);
    let label = format!("stash@{{{}}}  {}", stash.index, shorten(&stash.message, 44));
    let resp = ui.selectable_label(false, label);
    resp.clone().on_hover_text(format!(
        "{short}\n{}\n\nDouble-click to pop. Right-click for more.",
        stash.message
    ));

    // Double-click = pop (apply + drop). Fastest path for "I want this back".
    if resp.double_clicked() {
        *action = Some(CommitAction::StashPop { index: stash.index });
    }

    // Right-click context menu: Pop / Apply / Drop.
    resp.context_menu(|ui| {
        if ui
            .button("Pop")
            .on_hover_text("Apply and remove from stash list")
            .clicked()
        {
            *action = Some(CommitAction::StashPop { index: stash.index });
            ui.close_menu();
        }
        if ui
            .button("Apply")
            .on_hover_text("Apply without removing from stash list")
            .clicked()
        {
            *action = Some(CommitAction::StashApply { index: stash.index });
            ui.close_menu();
        }
        ui.separator();
        if ui
            .button(egui::RichText::new("Drop").color(egui::Color32::LIGHT_RED))
            .on_hover_text("Delete this stash entry")
            .clicked()
        {
            *action = Some(CommitAction::StashDropPrompt {
                index: stash.index,
                message: stash.message.clone(),
            });
            ui.close_menu();
        }
    });
}

fn short_sha(oid: &gix::ObjectId) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}

/// What `show()` needs to know about the LFS state, decoupled from the
/// owning `WorkspaceState` so the rest of the sidebar can borrow `app`
/// freely.
struct LfsViewState {
    running: bool,
    dismissed: bool,
    result: Option<LfsScanResult>,
}

fn render_lfs_section(ui: &mut egui::Ui, lfs: &LfsViewState, dismiss: &mut bool) {
    if lfs.dismissed {
        return;
    }
    if lfs.running && lfs.result.is_none() {
        // Subtle hint while the background scan is still in progress.
        // We don't open a dedicated section for it — that would feel
        // noisy when no problem may exist.
        ui.weak("Scanning for large files…");
        return;
    }
    let Some(result) = &lfs.result else {
        return;
    };
    if result.candidates.is_empty() {
        // All clear — nothing actionable, so don't add visual noise.
        return;
    }

    // Header is always-open (not collapsing) so the warning is visible.
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("⚠ Large tracked files")
                    .strong()
                    .color(egui::Color32::from_rgb(220, 170, 60)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("✕")
                    .on_hover_text("Dismiss for this session")
                    .clicked()
                {
                    *dismiss = true;
                }
            });
        });

        ui.add_space(2.0);
        ui.weak(format!(
            "{} file{} ≥ 10 MB committed without LFS.",
            result.candidates.len(),
            if result.candidates.len() == 1 {
                ""
            } else {
                "s"
            },
        ));
        ui.weak(
            "git checkout / undo / stash will all read & write the full \
             file. Consider `git lfs migrate import`.",
        );

        ui.add_space(4.0);
        // Show top 5; rest is collapsed under a "more" line.
        let top: Vec<&LfsCandidate> = result.candidates.iter().take(5).collect();
        for cand in &top {
            lfs_row(ui, cand);
        }
        if result.candidates.len() > top.len() {
            ui.weak(format!(
                "+ {} more file(s){}",
                result.candidates.len() - top.len(),
                if result.truncated {
                    " (scan stopped early; full list may be larger)"
                } else {
                    ""
                },
            ));
        } else if result.truncated {
            ui.weak("Scan stopped early; more candidates may exist.");
        }
    });
    ui.add_space(4.0);
}

fn lfs_row(ui: &mut egui::Ui, cand: &LfsCandidate) {
    let path_str = cand.path.display().to_string();
    let label = format!(
        "{}  {}",
        format_size_short(cand.size),
        shorten(&path_str, 40)
    );
    let resp = ui.label(egui::RichText::new(label).monospace().small());
    resp.on_hover_text(format!(
        "{}\n{} bytes\nblob {}",
        path_str, cand.size, cand.oid
    ));
}

fn format_size_short(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:>4.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:>4} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:>4} KB", bytes / KB)
    } else {
        format!("{:>4}  B", bytes)
    }
}

fn shorten(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

/// Truncate a path string from the left (head) rather than the tail
/// (end), so long paths still show their filename — `…game/art/hero.psd`
/// is more useful than `game/art/long/ni…` in a narrow sidebar.
fn shorten_path_left(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_owned();
    }
    // Keep the last `max_chars - 1` chars, prefix with an ellipsis
    // so the filename end is always visible.
    let skip = count - (max_chars.saturating_sub(1));
    let mut out = String::from("…");
    out.extend(s.chars().skip(skip));
    out
}

/// Snapshot of the "File locks" panel state the outer `show` loop
/// hands to [`render_lfs_locks_section`]. Cloned each frame so the
/// renderer doesn't borrow `WorkspaceState` across its body (the
/// rest of the sidebar needs that borrow dropped before we dispatch
/// intents).
struct LfsLocksViewState {
    /// `None` = we haven't tried a refresh yet; `Some(Vec::new())`
    /// = we tried and got no locks (possibly `Unavailable`).
    locks: Option<Vec<LfsLock>>,
    unavailable_reason: Option<String>,
    refreshing: bool,
    /// Cached `git config user.name` — compared case-sensitively
    /// against `LfsLock::owner` to decide whether to show a native
    /// "Unlock" button or a "Force unlock" affordance.
    current_user: String,
}

fn render_lfs_locks_section(
    ui: &mut egui::Ui,
    state: &LfsLocksViewState,
    intent: &mut Option<LockIntent>,
) {
    let lock_count = state.locks.as_ref().map(|v| v.len()).unwrap_or(0);
    let header = if lock_count == 0 {
        "File locks".to_string()
    } else {
        format!("File locks ({lock_count})")
    };

    egui::CollapsingHeader::new(header)
        .default_open(false)
        .show(ui, |ui| {
            // Refresh row at the top — spinner + button, same
            // pattern as the forge panel. The spinner makes it
            // obvious the refresh is actually in flight rather
            // than the button just being unresponsive.
            ui.horizontal(|ui| {
                let refresh_button = ui
                    .small_button("⟳ Refresh")
                    .on_hover_text("Re-fetch the list from the LFS lock server");
                if refresh_button.clicked() {
                    *intent = Some(LockIntent::Refresh);
                }
                if state.refreshing {
                    ui.add(egui::Spinner::new().size(12.0));
                    ui.weak(egui::RichText::new("refreshing…").small());
                }
            });
            ui.add_space(2.0);

            // Unavailable state: render inline, not a toast —
            // the user's repo may legitimately not participate in
            // LFS locks and we don't want to nag.
            if let Some(reason) = state.unavailable_reason.as_deref() {
                ui.weak(
                    egui::RichText::new(format!("LFS locks unavailable: {reason}"))
                        .small(),
                );
                return;
            }

            let locks = match state.locks.as_ref() {
                Some(l) => l,
                None => {
                    ui.weak(
                        egui::RichText::new("Loading lock list…")
                            .small(),
                    );
                    return;
                }
            };
            if locks.is_empty() {
                ui.weak(egui::RichText::new("No files locked.").small());
                return;
            }

            // The current-user comparison is empty-safe: an empty
            // cached name matches nobody, which means every row
            // renders as "owned by someone else". That's the
            // desired failure mode — worst case, the user gets
            // force-unlock prompts instead of plain unlocks; they
            // can still act, nothing is silently blocked.
            let me = state.current_user.as_str();
            for lock in locks {
                lock_row(ui, lock, me, intent);
            }
        });
    ui.add_space(4.0);
}

fn lock_row(
    ui: &mut egui::Ui,
    lock: &LfsLock,
    current_user: &str,
    intent: &mut Option<LockIntent>,
) {
    ui.horizontal(|ui| {
        // Lock glyph keeps the row scannable — rows are sorted by
        // the server but the eye still needs a fixed anchor.
        ui.label(egui::RichText::new("🔒").small());
        // Owner first because "who has this?" is usually the
        // scanner's first question on a shared team.
        let owner = if lock.owner.is_empty() {
            "(unknown)".to_string()
        } else {
            lock.owner.clone()
        };
        ui.label(
            egui::RichText::new(&owner)
                .small()
                .strong(),
        );
        ui.weak(egui::RichText::new("·").small());
        // Path truncated from the left so the filename end stays
        // visible in a narrow sidebar.
        let path_str = lock.path.display().to_string();
        let display = shorten_path_left(&path_str, 34);
        let path_label = ui
            .label(egui::RichText::new(&display).monospace().small())
            .on_hover_text(hover_for_lock(lock));

        // Unlock / Force unlock affordance on the right.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let is_mine = !current_user.is_empty() && lock.owner == current_user;
            if is_mine {
                if ui
                    .small_button("Unlock")
                    .on_hover_text("Release this lock on the server")
                    .clicked()
                {
                    *intent = Some(LockIntent::Unlock {
                        path: lock.path.clone(),
                    });
                }
            } else {
                // Force-unlock affordance appears when we row-hover.
                // We key it to hover on the path label so the button
                // doesn't compete with the path for tap area.
                if path_label.hovered() || ui.ui_contains_pointer() {
                    if ui
                        .small_button("Force unlock")
                        .on_hover_text(
                            "Admin-only: override another user's lock (requires server permission)",
                        )
                        .clicked()
                    {
                        *intent = Some(LockIntent::ForceUnlockPrompt {
                            path: lock.path.clone(),
                            owner: owner.clone(),
                        });
                    }
                }
            }
        });
    });
}

fn hover_for_lock(lock: &LfsLock) -> String {
    let mut out = format!("Locked by {}", lock.owner);
    if let Some(ts) = lock.locked_at.as_deref() {
        out.push_str(&format!("\nat {ts}"));
    }
    if !lock.id.is_empty() {
        out.push_str(&format!("\nlock id: {}", lock.id));
    }
    out.push_str(&format!("\n{}", lock.path.display()));
    out
}

/// Render the force-unlock confirmation window. Called from the
/// main update loop alongside the other modal renderers so users
/// don't miss the affordance on a background tab.
pub fn show_force_unlock_confirm(ctx: &egui::Context, app: &mut MergeFoxApp) {
    let Some(confirm) = app.force_unlock_confirm.as_ref() else {
        return;
    };
    let path = confirm.path.clone();
    let owner = confirm.owner.clone();
    let mut close = false;
    let mut submitted = false;
    egui::Window::new("Force unlock?")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .default_width(360.0)
        .show(ctx, |ui| {
            ui.label(format!(
                "This overrides another user's lock. Continue?\n\nPath: {}\nOwner: {}",
                path.display(),
                owner
            ));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    close = true;
                }
                if ui
                    .button(
                        egui::RichText::new("Force unlock")
                            .color(egui::Color32::LIGHT_RED),
                    )
                    .clicked()
                {
                    submitted = true;
                }
            });
        });
    if close {
        app.force_unlock_confirm = None;
    } else if submitted {
        app.force_unlock_confirm = None;
        app.start_lfs_unlock(path, true);
    }
}
