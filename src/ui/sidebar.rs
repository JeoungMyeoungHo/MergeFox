use crate::actions::CommitAction;
use crate::app::{MergeFoxApp, View};
use crate::git::{BranchInfo, LfsCandidate, LfsScanResult, StashEntry};

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    let sidebar_fill = crate::ui::theme::sidebar_fill(&app.config.theme);
    let language = app.config.ui_language;

    let (branch_error, local, remote, configured_remotes, stashes, current_branch, head_branch, forge, lfs) = {
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
        let (branch_error, branches, stashes) = match &ws.repo_ui_cache {
            Some(cache) => (
                cache.branch_error.clone(),
                cache.branches.clone(),
                cache.stashes.clone(),
            ),
            None => (None, Vec::new(), None),
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

        (
            branch_error,
            local,
            remote,
            configured_remotes,
            stashes,
            ws.selected_branch.clone(),
            head_branch,
            ws.forge.clone(),
            lfs,
        )
    };

    let mut select_branch: Option<String> = None;
    let mut forge_action: Option<crate::ui::forge::SidebarAction> = None;
    let mut dismiss_lfs = false;
    let mut stash_action: Option<CommitAction> = None;
    let mut branch_action: Option<CommitAction> = None;
    let mut open_publish_remote: Option<String> = None;

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
                    for branch in &local {
                        branch_row(
                            ui,
                            branch,
                            current_branch.as_deref(),
                            configured_remotes.is_empty() && branch.is_head && branch.upstream.is_none(),
                            &mut select_branch,
                            &mut branch_action,
                            &mut open_publish_remote,
                        );
                    }
                });
                ui.collapsing("Remote", |ui| {
                    if configured_remotes.is_empty() {
                        ui.weak("No remotes configured yet.");
                        if let Some(branch) = head_branch.as_deref() {
                            if ui.small_button("Publish current branch…").clicked() {
                                open_publish_remote = Some(branch.to_string());
                            }
                        }
                        ui.add_space(4.0);
                    } else if remote.is_empty() {
                        ui.weak("No remote-tracking branches yet.");
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

                        if let Some(stashes) = &stashes {
                            if stashes.is_empty() {
                                ui.weak("No stashes.");
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
