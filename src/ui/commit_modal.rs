//! Commit dialog.
//!
//! Layout: two file panels (Unstaged / Staged) with per-file checkboxes +
//! move buttons, a message editor below, and commit / amend actions at the
//! bottom. Selection is persistent across status polls so a rebuild doesn't
//! drop the user's pending "move these three" intent.
//!
//!   +--------------------------- Commit ---------------------------+
//!   |  [🗂 Unstaged files   (N)]                ⬇ Stage selected  |
//!   |    [✓] M src/a.rs                          ⬇ Stage all       |
//!   |    [ ] A src/b.rs                                            |
//!   |  ----------------------------------------------------------- |
//!   |  [📦 Staged files     (M)]                ⬆ Unstage selected |
//!   |    [ ] M src/c.rs                          ⬆ Unstage all     |
//!   |                                                              |
//!   |  Message: [__________________________]                       |
//!   |  [✨ Generate]                                               |
//!   |                                                              |
//!   |  [Cancel]                  [Amend last]  [Commit staged ▸]  |
//!   +--------------------------------------------------------------+

use std::path::{Path, PathBuf};

use egui::{Color32, RichText};

use crate::app::{CommitModal, MergeFoxApp, View};
use crate::git::{EntryKind, StatusEntry};

const COMMIT_AI_DIFF_BYTES: usize = 16_384;

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    if !app.commit_modal_open {
        return;
    }

    let View::Workspace(tabs) = &app.view else {
        app.commit_modal_open = false;
        return;
    };
    let ws = tabs.current();

    let entries = match crate::git::ops::status_entries(ws.repo.path()) {
        Ok(v) => v,
        Err(e) => {
            app.last_error = Some(format!("status: {e:#}"));
            app.commit_modal_open = false;
            return;
        }
    };

    // AI state snapshot (read-only capture so the window closure can
    // render without re-borrowing `app`).
    let app_has_ai_endpoint_snapshot = app.config.ai_endpoint.is_some();
    let ai_in_flight_snapshot = app.commit_ai_task.is_some();
    let ai_error_snapshot = app.commit_modal.as_ref().and_then(|m| m.ai_error.clone());

    let staged_count = entries.iter().filter(|e| e.staged).count();
    let unstaged_count = entries
        .iter()
        .filter(|e| e.unstaged || matches!(e.kind, EntryKind::Untracked))
        .count();

    let recent_messages = ws
        .repo
        .linear_head_commits(12)
        .ok()
        .map(|commits| {
            let mut seen = std::collections::BTreeSet::new();
            commits
                .into_iter()
                .rev()
                .filter_map(|commit| {
                    let message = commit.message.trim().to_owned();
                    if message.is_empty() || !seen.insert(message.clone()) {
                        None
                    } else {
                        Some(message)
                    }
                })
                .take(8)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut open = true;
    let mut result: CommitIntent = CommitIntent::None;
    let mut move_intent: Option<MoveIntent> = None;
    let mut open_ai_settings = false;

    // Prune stale selections (files that disappeared from the status list,
    // e.g. just got committed in another pane) so the selection counter
    // stays honest.
    let commit_modal = app.commit_modal.get_or_insert_with(CommitModal::default);
    sync_amend_author_state(commit_modal, ws.repo.path());
    let valid_paths: std::collections::BTreeSet<PathBuf> =
        entries.iter().map(|e| e.path.clone()).collect();
    commit_modal.selection.retain(|p| valid_paths.contains(p));
    if commit_modal
        .selection_anchor
        .as_ref()
        .is_some_and(|p| !valid_paths.contains(p))
    {
        commit_modal.selection_anchor = None;
    }

    egui::Window::new("Commit")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(720.0)
        .min_width(560.0)
        .show(ctx, |ui| {
            // ---- Unstaged panel -----------------------------------------
            render_panel(
                ui,
                PanelKind::Unstaged,
                &entries,
                commit_modal,
                &mut move_intent,
                unstaged_count,
            );

            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);

            // ---- Staged panel -------------------------------------------
            render_panel(
                ui,
                PanelKind::Staged,
                &entries,
                commit_modal,
                &mut move_intent,
                staged_count,
            );

            ui.add_space(8.0);
            ui.separator();

            // ---- Message editor -----------------------------------------
            ui.label(RichText::new("Message").strong());
            ui.add(
                egui::TextEdit::multiline(&mut commit_modal.message)
                    .hint_text("Subject line\n\nOptional longer description…")
                    .desired_rows(5)
                    .desired_width(f32::INFINITY)
                    .font(egui::TextStyle::Monospace),
            );

            if !recent_messages.is_empty() {
                ui.add_space(6.0);
                ui.label("Recent commit messages:");
                ui.horizontal_wrapped(|ui| {
                    for message in &recent_messages {
                        let subject = message.lines().next().unwrap_or(message);
                        if ui.button(subject).clicked() {
                            commit_modal.message = message.clone();
                            commit_modal.last_error = None;
                        }
                    }
                });
            }

            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);

            ui.label(RichText::new("Amend author").strong());
            if commit_modal.amend_head_available {
                ui.weak(format!(
                    "Current HEAD author: {} <{}>",
                    commit_modal.amend_head_author_name, commit_modal.amend_head_author_email
                ));
                ui.add_space(4.0);
                ui.checkbox(
                    &mut commit_modal.amend_author_override,
                    "Change author when amending",
                );
                if commit_modal.amend_author_override {
                    ui.add_space(4.0);
                    ui.label("Author name:");
                    ui.text_edit_singleline(&mut commit_modal.amend_author_name);
                    ui.label("Author email:");
                    ui.text_edit_singleline(&mut commit_modal.amend_author_email);
                }
            } else {
                ui.weak("No existing HEAD commit yet. Create the first commit before using Amend.");
            }

            ui.add_space(6.0);

            if let Some(err) = &commit_modal.last_error {
                ui.colored_label(Color32::LIGHT_RED, err);
                ui.add_space(4.0);
            }

            // ---- AI generate row ----------------------------------------
            ui.horizontal(|ui| {
                let button = egui::Button::new(if ai_in_flight_snapshot {
                    "⏳ Generating…"
                } else {
                    "✨ Generate message"
                });
                let resp = ui.add_enabled(!ai_in_flight_snapshot, button);
                if !app_has_ai_endpoint_snapshot {
                    resp.clone()
                        .on_hover_text("Configure an AI endpoint in Settings → AI first.");
                    if resp.clicked() {
                        result = CommitIntent::GenerateMessage;
                    }
                    if ui.button("AI settings…").clicked() {
                        open_ai_settings = true;
                    }
                } else if ai_in_flight_snapshot {
                    ui.spinner();
                } else if resp.clicked() {
                    result = CommitIntent::GenerateMessage;
                }
                if let Some(err) = &ai_error_snapshot {
                    ui.colored_label(Color32::LIGHT_RED, format!("AI: {err}"));
                }
            });

            ui.add_space(6.0);

            // ---- Commit / Amend buttons ---------------------------------
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    result = CommitIntent::Cancel;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let msg_ok = !commit_modal.message.trim().is_empty();
                    let any_staged = staged_count > 0;
                    let amend_author_ok = !commit_modal.amend_author_override
                        || (!commit_modal.amend_author_name.trim().is_empty()
                            && !commit_modal.amend_author_email.trim().is_empty());
                    let can_amend = msg_ok && commit_modal.amend_head_available && amend_author_ok;

                    ui.add_enabled_ui(can_amend, |ui| {
                        if ui
                            .button("Amend last")
                            .on_hover_text(
                                "Re-commit HEAD with the current staged tree, message, and optional author change",
                            )
                            .clicked()
                        {
                            match amend_author_override(commit_modal) {
                                Ok(author) => {
                                    result = CommitIntent::Amend {
                                        message: commit_modal.message.clone(),
                                        author,
                                    };
                                }
                                Err(err) => {
                                    commit_modal.last_error = Some(err);
                                }
                            }
                        }
                    });

                    let commit_btn = egui::Button::new(
                        RichText::new(if any_staged {
                            format!("▸ Commit {staged_count} staged")
                        } else {
                            "▸ Commit".to_string()
                        })
                        .color(Color32::WHITE)
                        .strong(),
                    )
                    .fill(if msg_ok && any_staged {
                        Color32::from_rgb(80, 160, 90)
                    } else {
                        Color32::DARK_GRAY
                    });
                    ui.add_enabled_ui(msg_ok && any_staged, |ui| {
                        if ui
                            .add(commit_btn)
                            .on_hover_text("Commit the staged files with the message above")
                            .clicked()
                        {
                            result = CommitIntent::CommitStaged(commit_modal.message.clone());
                        }
                    });

                    ui.add_enabled_ui(msg_ok, |ui| {
                        if ui
                            .button("Stage all & commit")
                            .on_hover_text(
                                "Convenience: stage every change (incl. untracked) first, then commit",
                            )
                            .clicked()
                        {
                            result = CommitIntent::StageAllAndCommit(commit_modal.message.clone());
                        }
                    });
                });
            });
        });

    if !open {
        app.commit_modal_open = false;
        if let Some(commit_modal) = app.commit_modal.as_mut() {
            reset_amend_author_state(commit_modal);
        }
    }
    if matches!(result, CommitIntent::Cancel) {
        app.commit_modal_open = false;
        if let Some(commit_modal) = app.commit_modal.as_mut() {
            reset_amend_author_state(commit_modal);
        }
    }

    // Apply per-file move actions (stage / unstage) BEFORE handling
    // commit-level intent so the next frame's status poll picks up the
    // new state.
    if let Some(intent) = move_intent {
        apply_move_intent(app, intent);
    }
    if open_ai_settings {
        app.open_settings_section(crate::ui::settings::SettingsSection::Ai);
    }

    match result {
        CommitIntent::None | CommitIntent::Cancel => {}
        CommitIntent::GenerateMessage => start_ai_generation(app),
        other => handle_commit_intent(app, other),
    }

    poll_ai_task(app);
}

#[derive(Clone, Copy, PartialEq)]
enum PanelKind {
    Unstaged,
    Staged,
}

enum MoveIntent {
    /// Stage the specific selected paths.
    Stage(Vec<PathBuf>),
    /// Unstage the specific selected paths.
    Unstage(Vec<PathBuf>),
    /// `git add -A` for every unstaged path.
    StageAll,
    /// `git reset HEAD` for every staged path.
    UnstageAll,
    /// Single-row toggle via arrow icon.
    StageOne(PathBuf),
    UnstageOne(PathBuf),
    /// Discard the given unstaged paths. Tracked files restore from index;
    /// untracked files are removed from disk.
    Discard(Vec<DiscardPath>),
}

#[derive(Clone)]
struct DiscardPath {
    path: PathBuf,
    untracked: bool,
}

/// Render the Unstaged or Staged panel with its own header (count + action
/// buttons) and a scrollable file list.
fn render_panel(
    ui: &mut egui::Ui,
    kind: PanelKind,
    entries: &[StatusEntry],
    modal: &mut CommitModal,
    move_intent: &mut Option<MoveIntent>,
    count_for_header: usize,
) {
    // Filter entries belonging to this panel.
    //
    // A file can be in BOTH panels (staged tweaks + further unstaged
    // edits). In that case we show the row in both — clicking "stage" on
    // the unstaged half adds the new tweaks to the index without touching
    // the already-staged portion.
    let belongs: Vec<&StatusEntry> = entries
        .iter()
        .filter(|e| match kind {
            PanelKind::Unstaged => {
                e.unstaged || matches!(e.kind, EntryKind::Untracked) || e.conflicted
            }
            PanelKind::Staged => e.staged,
        })
        .collect();

    // Selected-in-panel count (selection is shared across panels but
    // actions only apply to paths visible in the current panel).
    let selected_in_panel: Vec<PathBuf> = belongs
        .iter()
        .filter(|e| modal.selection.contains(&e.path))
        .map(|e| e.path.clone())
        .collect();
    let selected_in_panel_count = selected_in_panel.len();
    let discardable_in_panel: Vec<DiscardPath> = belongs.iter().map(|e| discard_path(e)).collect();
    let selected_discardable: Vec<DiscardPath> = belongs
        .iter()
        .filter(|e| modal.selection.contains(&e.path))
        .map(|e| discard_path(e))
        .collect();

    // Header row: title + counts + bulk action buttons.
    ui.horizontal(|ui| {
        let (icon, title) = match kind {
            PanelKind::Unstaged => ("🗂", "Unstaged"),
            PanelKind::Staged => ("📦", "Staged"),
        };
        ui.label(
            RichText::new(format!("{icon} {title}  ({count_for_header})"))
                .strong()
                .heading(),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Per-panel "select all in this panel" toggle. Visible only
            // when there are rows to act on.
            let all_selected =
                !belongs.is_empty() && belongs.iter().all(|e| modal.selection.contains(&e.path));
            match kind {
                PanelKind::Unstaged => {
                    // Primary action: stage
                    ui.add_enabled_ui(count_for_header > 0, |ui| {
                        if ui
                            .button("⬇ Stage all")
                            .on_hover_text(
                                "git add -A — stage every unstaged change, incl. untracked",
                            )
                            .clicked()
                        {
                            *move_intent = Some(MoveIntent::StageAll);
                        }
                    });
                    ui.add_enabled_ui(selected_in_panel_count > 0, |ui| {
                        let label = format!("⬇ Stage selected ({selected_in_panel_count})");
                        if ui
                            .button(label)
                            .on_hover_text("Stage only the checked files")
                            .clicked()
                        {
                            *move_intent = Some(MoveIntent::Stage(selected_in_panel.clone()));
                        }
                    });
                    ui.add_enabled_ui(!discardable_in_panel.is_empty(), |ui| {
                        if ui
                            .button(
                                RichText::new("Discard all")
                                    .color(Color32::from_rgb(232, 120, 120)),
                            )
                            .on_hover_text(
                                "Discard every unstaged change. Tracked files restore from the index; untracked files are removed.",
                            )
                            .clicked()
                        {
                            *move_intent = Some(MoveIntent::Discard(discardable_in_panel.clone()));
                        }
                    });
                    ui.add_enabled_ui(!selected_discardable.is_empty(), |ui| {
                        let label = format!("Discard selected ({})", selected_discardable.len());
                        if ui
                            .button(
                                RichText::new(label)
                                    .color(Color32::from_rgb(232, 120, 120)),
                            )
                            .on_hover_text("Discard only the checked unstaged files")
                            .clicked()
                        {
                            *move_intent = Some(MoveIntent::Discard(selected_discardable.clone()));
                        }
                    });
                }
                PanelKind::Staged => {
                    ui.add_enabled_ui(count_for_header > 0, |ui| {
                        if ui
                            .button("⬆ Unstage all")
                            .on_hover_text("git reset HEAD — move every staged change back")
                            .clicked()
                        {
                            *move_intent = Some(MoveIntent::UnstageAll);
                        }
                    });
                    ui.add_enabled_ui(selected_in_panel_count > 0, |ui| {
                        let label = format!("⬆ Unstage selected ({selected_in_panel_count})");
                        if ui
                            .button(label)
                            .on_hover_text("Unstage only the checked files")
                            .clicked()
                        {
                            *move_intent = Some(MoveIntent::Unstage(selected_in_panel.clone()));
                        }
                    });
                }
            }

            // "Select all" toggle checkbox on the right.
            if !belongs.is_empty() {
                let mut checked = all_selected;
                if ui
                    .checkbox(&mut checked, RichText::new("All").small())
                    .on_hover_text("Select every file in this panel")
                    .changed()
                {
                    if checked {
                        for e in &belongs {
                            modal.selection.insert(e.path.clone());
                        }
                    } else {
                        for e in &belongs {
                            modal.selection.remove(&e.path);
                        }
                    }
                }
            }
        });
    });

    ui.add_space(2.0);

    // Scrollable file list.
    egui::ScrollArea::vertical()
        .id_salt(match kind {
            PanelKind::Unstaged => "commit_unstaged_scroll",
            PanelKind::Staged => "commit_staged_scroll",
        })
        .max_height(180.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            if belongs.is_empty() {
                ui.weak(match kind {
                    PanelKind::Unstaged => "No unstaged changes.",
                    PanelKind::Staged => "Nothing staged yet.",
                });
                return;
            }
            for entry in &belongs {
                render_row(ui, kind, &belongs, entry, modal, move_intent);
            }
        });
}

/// Render one file row: [checkbox] [glyph] path     [move-arrow]
fn render_row(
    ui: &mut egui::Ui,
    kind: PanelKind,
    panel_entries: &[&StatusEntry],
    entry: &StatusEntry,
    modal: &mut CommitModal,
    move_intent: &mut Option<MoveIntent>,
) {
    ui.horizontal(|ui| {
        let mut checked = modal.selection.contains(&entry.path);
        if ui.checkbox(&mut checked, "").changed() {
            apply_selection_click(ui, panel_entries, entry, modal);
        }

        let (color, glyph) = style_for(&entry.kind, entry.staged, entry.unstaged);
        ui.label(RichText::new(glyph).color(color).monospace().strong());

        // Clickable path label — clicking toggles the checkbox so users
        // don't have to aim at the tiny checkbox.
        let path_text = entry.path.display().to_string();
        if ui
            .add(egui::Label::new(path_text).sense(egui::Sense::click()))
            .clicked()
        {
            apply_selection_click(ui, panel_entries, entry, modal);
        }

        // Per-row move arrow on the right edge.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            match kind {
                PanelKind::Unstaged => {
                    if ui
                        .small_button("⬇")
                        .on_hover_text("Stage this file")
                        .clicked()
                    {
                        *move_intent = Some(MoveIntent::StageOne(entry.path.clone()));
                    }
                }
                PanelKind::Staged => {
                    if ui
                        .small_button("⬆")
                        .on_hover_text("Unstage this file")
                        .clicked()
                    {
                        *move_intent = Some(MoveIntent::UnstageOne(entry.path.clone()));
                    }
                }
            }
            // Secondary flag: "also has unstaged tweaks on top of staged"
            if kind == PanelKind::Staged && entry.unstaged && entry.staged {
                ui.weak(RichText::new("+unstaged").small());
            }
            if entry.conflicted {
                ui.colored_label(Color32::from_rgb(240, 90, 90), "⚠ conflicted");
            }
        });
    });
}

fn apply_selection_click(
    ui: &egui::Ui,
    panel_entries: &[&StatusEntry],
    clicked: &StatusEntry,
    modal: &mut CommitModal,
) {
    let modifiers = ui.input(|i| i.modifiers);
    if modifiers.shift {
        let mode = if modifiers.alt {
            RangeSelectionMode::Deselect
        } else {
            RangeSelectionMode::Select
        };
        apply_range_selection(panel_entries, &clicked.path, modal, mode);
    } else if modal.selection.contains(&clicked.path) {
        modal.selection.remove(&clicked.path);
    } else {
        modal.selection.insert(clicked.path.clone());
    }
    modal.selection_anchor = Some(clicked.path.clone());
}

#[derive(Clone, Copy)]
enum RangeSelectionMode {
    Select,
    Deselect,
}

fn apply_range_selection(
    panel_entries: &[&StatusEntry],
    clicked_path: &std::path::Path,
    modal: &mut CommitModal,
    mode: RangeSelectionMode,
) {
    let Some(clicked_idx) = panel_entries.iter().position(|e| e.path == clicked_path) else {
        return;
    };
    let anchor_idx = modal
        .selection_anchor
        .as_ref()
        .and_then(|anchor| panel_entries.iter().position(|e| &e.path == anchor))
        .unwrap_or(clicked_idx);

    let (start, end) = if anchor_idx <= clicked_idx {
        (anchor_idx, clicked_idx)
    } else {
        (clicked_idx, anchor_idx)
    };

    for entry in &panel_entries[start..=end] {
        match mode {
            RangeSelectionMode::Select => {
                modal.selection.insert(entry.path.clone());
            }
            RangeSelectionMode::Deselect => {
                modal.selection.remove(&entry.path);
            }
        }
    }
}

/// Execute a stage/unstage intent against the working copy. Errors surface
/// on the commit modal so the user can retry.
fn apply_move_intent(app: &mut MergeFoxApp, intent: MoveIntent) {
    let View::Workspace(tabs) = &mut app.view else {
        return;
    };
    let ws = tabs.current_mut();
    let path = ws.repo.path().to_path_buf();

    let result: anyhow::Result<()> = match intent {
        MoveIntent::StageAll => crate::git::ops::stage_all(&path).map(|_| ()),
        MoveIntent::UnstageAll => crate::git::cli::run(&path, ["reset", "HEAD", "--"]).map(|_| ()),
        MoveIntent::Stage(paths) => {
            let refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_path()).collect();
            crate::git::ops::stage_paths(&path, &refs)
        }
        MoveIntent::Unstage(paths) => {
            let refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_path()).collect();
            crate::git::ops::unstage_paths(&path, &refs)
        }
        MoveIntent::StageOne(p) => {
            let refs: Vec<&std::path::Path> = vec![p.as_path()];
            crate::git::ops::stage_paths(&path, &refs)
        }
        MoveIntent::UnstageOne(p) => {
            let refs: Vec<&std::path::Path> = vec![p.as_path()];
            crate::git::ops::unstage_paths(&path, &refs)
        }
        MoveIntent::Discard(paths) => {
            let tracked: Vec<&std::path::Path> = paths
                .iter()
                .filter(|p| !p.untracked)
                .map(|p| p.path.as_path())
                .collect();
            let untracked: Vec<&std::path::Path> = paths
                .iter()
                .filter(|p| p.untracked)
                .map(|p| p.path.as_path())
                .collect();
            crate::git::ops::discard_paths(&path, &tracked, &untracked)
        }
    };
    if let Err(e) = result {
        if let Some(m) = app.commit_modal.as_mut() {
            m.last_error = Some(format!("{e:#}"));
        }
    } else if let Some(m) = app.commit_modal.as_mut() {
        m.last_error = None;
    }
    // Refresh the repo UI cache so the sidebar/main panel pick up the
    // new status on the next frame.
    app.refresh_repo_ui_cache();
}

fn discard_path(entry: &StatusEntry) -> DiscardPath {
    DiscardPath {
        path: entry.path.clone(),
        untracked: matches!(entry.kind, EntryKind::Untracked),
    }
}

// -------------------------- AI plumbing (unchanged) -------------------------

fn start_ai_generation(app: &mut MergeFoxApp) {
    let endpoint = match app.config.ai_endpoint.clone() {
        Some(mut ep) => match app.secret_store.load_api_key(&ep.name) {
            Ok(key) => {
                ep.api_key = key;
                ep
            }
            Err(e) => {
                if let Some(m) = app.commit_modal.as_mut() {
                    m.ai_error = Some(format!("secret store: {e:#}"));
                }
                return;
            }
        },
        None => {
            if let Some(m) = app.commit_modal.as_mut() {
                m.ai_error = Some("No AI endpoint configured.".to_string());
            }
            app.hud = Some(crate::app::Hud::with_action(
                "AI isn't configured yet.",
                2600,
                "Open AI settings",
                crate::app::HudIntent::OpenSettings(crate::ui::settings::SettingsSection::Ai),
            ));
            return;
        }
    };

    let diff = {
        let View::Workspace(tabs) = &app.view else {
            return;
        };
        let ws = tabs.current();
        match ws.repo.staged_diff_text(COMMIT_AI_DIFF_BYTES) {
            Ok(d) if d.trim().is_empty() => {
                if let Some(m) = app.commit_modal.as_mut() {
                    m.ai_error =
                        Some("Nothing to diff — stage or modify some files first.".to_string());
                }
                return;
            }
            Ok(d) => d,
            Err(e) => {
                if let Some(m) = app.commit_modal.as_mut() {
                    m.ai_error = Some(format!("diff: {e:#}"));
                }
                return;
            }
        }
    };

    if let Some(m) = app.commit_modal.as_mut() {
        m.ai_error = None;
    }

    let task = crate::ai::AiTask::spawn(async move {
        let client = crate::ai::build_client(endpoint);
        let opts = crate::ai::tasks::commit_message::CommitMessageOpts::default();
        crate::ai::tasks::commit_message::gen_commit_message(client.as_ref(), &diff, opts).await
    });
    app.commit_ai_task = Some(task);
}

fn poll_ai_task(app: &mut MergeFoxApp) {
    let Some(task) = app.commit_ai_task.as_mut() else {
        return;
    };
    let Some(result) = task.poll() else {
        return;
    };
    app.commit_ai_task = None;

    let Some(modal) = app.commit_modal.as_mut() else {
        return;
    };
    match result {
        Ok(sugg) => {
            let suggestion = format_suggestion(&sugg);
            if modal.message.trim().is_empty() {
                modal.message = suggestion;
            } else {
                modal.message =
                    format!("{}\n\n--- AI suggestion ---\n{}", modal.message, suggestion);
            }
            modal.ai_error = None;
        }
        Err(e) => {
            modal.ai_error = Some(format!("{e}"));
        }
    }
}

fn format_suggestion(sugg: &crate::ai::tasks::commit_message::CommitSuggestion) -> String {
    match &sugg.body {
        Some(body) => format!("{}\n\n{}", sugg.title, body),
        None => sugg.title.clone(),
    }
}

// -------------------------- commit intent handling --------------------------

enum CommitIntent {
    None,
    Cancel,
    /// Commit whatever's currently staged (no auto-stage).
    CommitStaged(String),
    /// Convenience: stage every change, then commit.
    StageAllAndCommit(String),
    Amend {
        message: String,
        author: Option<crate::git::ops::CommitAuthor>,
    },
    GenerateMessage,
}

fn handle_commit_intent(app: &mut MergeFoxApp, intent: CommitIntent) {
    use crate::journal::{self, Operation};

    let mut close_modal = false;
    let mut hud_msg: Option<String> = None;
    let mut journal_entry: Option<(Operation, journal::RepoSnapshot, journal::RepoSnapshot)> = None;
    let mut err: Option<String> = None;
    let mut rebuild = None;

    {
        let View::Workspace(tabs) = &mut app.view else {
            return;
        };
        let ws = tabs.current_mut();

        let before = journal::capture(ws.repo.path()).ok();

        let outcome: Result<(String, Operation), anyhow::Error> = match intent {
            CommitIntent::CommitStaged(msg) => {
                crate::git::ops::commit(ws.repo.path(), &msg).map(|oid| {
                    (
                        format!("Committed {}", short(&oid)),
                        Operation::Commit {
                            message: msg,
                            amended: false,
                        },
                    )
                })
            }
            CommitIntent::StageAllAndCommit(msg) => crate::git::ops::stage_all(ws.repo.path())
                .and_then(|_| crate::git::ops::commit(ws.repo.path(), &msg))
                .map(|oid| {
                    (
                        format!("Committed {}", short(&oid)),
                        Operation::Commit {
                            message: msg,
                            amended: false,
                        },
                    )
                }),
            CommitIntent::Amend { message, author } => {
                crate::git::ops::amend(ws.repo.path(), Some(&message), author.as_ref()).map(|oid| {
                    (
                        format!("Amended {}", short(&oid)),
                        Operation::Commit {
                            message,
                            amended: true,
                        },
                    )
                })
            }
            CommitIntent::GenerateMessage | CommitIntent::None | CommitIntent::Cancel => {
                unreachable!("these are handled earlier")
            }
        };

        match outcome {
            Ok((label, op)) => {
                if let (Some(b), Ok(a)) = (before, journal::capture(ws.repo.path())) {
                    journal_entry = Some((op, b, a));
                }
                hud_msg = Some(label);
                rebuild = Some(ws.graph_scope);
                close_modal = true;
            }
            Err(e) => {
                err = Some(format!("{e:#}"));
            }
        }
    }

    if let Some((op, before, after)) = journal_entry {
        app.journal_record(op, before, after);
    }
    if let Some(scope) = rebuild {
        app.rebuild_graph(scope);
    }
    if let Some(msg) = hud_msg {
        app.hud = Some(crate::app::Hud::new(msg, 1600));
    }
    if close_modal {
        app.commit_modal_open = false;
        if let Some(cm) = app.commit_modal.as_mut() {
            cm.message.clear();
            cm.last_error = None;
            cm.ai_error = None;
            cm.selection.clear();
            cm.selection_anchor = None;
            reset_amend_author_state(cm);
        }
    }
    if let Some(e) = err {
        if let Some(cm) = app.commit_modal.as_mut() {
            cm.last_error = Some(e);
        }
    }
}

fn sync_amend_author_state(modal: &mut CommitModal, repo_path: &Path) {
    if modal.amend_author_repo_path.as_deref() == Some(repo_path) {
        return;
    }
    reset_amend_author_state(modal);
    modal.amend_author_repo_path = Some(repo_path.to_path_buf());
    if let Ok(author) = crate::git::ops::head_commit_author(repo_path) {
        modal.amend_head_available = true;
        modal.amend_head_author_name = author.name.clone();
        modal.amend_head_author_email = author.email.clone();
        modal.amend_author_name = author.name;
        modal.amend_author_email = author.email;
    }
}

fn reset_amend_author_state(modal: &mut CommitModal) {
    modal.amend_author_repo_path = None;
    modal.amend_head_available = false;
    modal.amend_head_author_name.clear();
    modal.amend_head_author_email.clear();
    modal.amend_author_override = false;
    modal.amend_author_name.clear();
    modal.amend_author_email.clear();
}

fn amend_author_override(
    modal: &CommitModal,
) -> Result<Option<crate::git::ops::CommitAuthor>, String> {
    if !modal.amend_author_override {
        return Ok(None);
    }
    let author = crate::git::ops::CommitAuthor::normalized(
        &modal.amend_author_name,
        &modal.amend_author_email,
    )
    .map_err(|e| format!("amend author: {e:#}"))?;
    let unchanged = modal.amend_head_author_name.trim() == author.name
        && modal.amend_head_author_email.trim() == author.email;
    if unchanged {
        Ok(None)
    } else {
        Ok(Some(author))
    }
}

// -------------------------- row styling --------------------------

fn style_for(kind: &EntryKind, staged: bool, _unstaged: bool) -> (Color32, String) {
    let base_color = match kind {
        EntryKind::New | EntryKind::Untracked => Color32::from_rgb(90, 180, 120),
        EntryKind::Modified => Color32::from_rgb(220, 190, 90),
        EntryKind::Deleted => Color32::from_rgb(220, 100, 100),
        EntryKind::Renamed => Color32::from_rgb(150, 150, 220),
        EntryKind::Typechange => Color32::from_rgb(200, 120, 200),
        EntryKind::Conflicted => Color32::from_rgb(255, 80, 80),
    };
    let color = if staged {
        base_color
    } else {
        Color32::from_rgba_unmultiplied(base_color.r(), base_color.g(), base_color.b(), 160)
    };
    (color, kind.glyph().to_string())
}

fn short(oid: &gix::ObjectId) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}
