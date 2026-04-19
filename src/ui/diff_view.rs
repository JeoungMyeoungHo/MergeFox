//! Right-side diff viewer panel.
//!
//! Layout when a commit is selected:
//!
//! ```text
//!   ┌─────────────────── diff panel ───────────────────┐
//!   │ abc1234 → def5678   (3 files, +42 −17)           │  ← header
//!   ├───────────────────────────────────────────────────┤
//!   │ ▾ Files                                           │
//!   │   M  src/foo.rs       (+8 −2)                    │
//!   │   A  assets/icon.png                             │
//!   │   D  src/old.rs       (+0 −44)                   │
//!   ├───────────────────────────────────────────────────┤
//!   │ (unified diff / image side-by-side / binary msg)  │
//!   └───────────────────────────────────────────────────┘
//! ```
//!
//! Text diff is rendered as a unified-style listing with monospace font.
//! Image diff shows the old and new blobs side-by-side using egui's
//! image loaders (enabled via `egui_extras::install_image_loaders`).

use std::sync::Arc;

use egui::{Color32, FontId, Layout, RichText, ScrollArea, Stroke, Vec2};

use crate::app::{
    MergeFoxApp, SelectedFileView, SelectedImageCache, SnapshotCache, View, WorkspaceState,
};
use crate::git::{
    DeltaStatus, DiffLine, EntryKind, FileDiff, FileKind, LineKind, RepoDiff, StatusEntry,
};

const PANEL_MIN_WIDTH: f32 = 280.0;
const PANEL_DEFAULT_WIDTH: f32 = 340.0;
/// Default row height for file list rows *without* a thumbnail slot.
/// Matches the pre-thumbnail-era height so text-heavy file lists keep
/// their dense layout and don't visibly expand when this feature lands.
const FILE_ROW_HEIGHT: f32 = 20.0;
/// Row height when at least one file in the batch has an inline
/// thumbnail. We picked a single list-wide height (not per-row
/// adaptive) because `ScrollArea::show_rows` assumes a uniform row
/// height — mixed heights would force us off the fast virtualized
/// renderer. 28 px fits a 22 px thumbnail + 3 px breathing room and
/// reads well against the 12.5 px monospace path text.
const FILE_ROW_HEIGHT_WITH_THUMB: f32 = 28.0;
/// Rendered thumbnail size. Smaller than the decoded `THUMB_MAX_DIM`
/// (64 px) so even a tall/narrow asset downscales cleanly when drawn
/// at row height. We go slightly under the row height so a 1 px border
/// doesn't push the text baseline around.
const THUMB_DRAW_SIZE: f32 = 22.0;

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    let needs_image_loaders = match &app.view {
        View::Workspace(tabs) => {
            let ws = tabs.current();
            ws.current_diff
                .as_ref()
                .and_then(|diff| ws.selected_file_idx.and_then(|idx| diff.files.get(idx)))
                .map(|file| matches!(file.kind, FileKind::Image { .. }))
                .unwrap_or(false)
                || ws
                    .selected_working_file
                    .as_ref()
                    .map(|path| path_looks_like_image(path))
                    .unwrap_or(false)
        }
        _ => false,
    };
    if needs_image_loaders {
        app.ensure_image_loaders(ctx);
    }

    let View::Workspace(tabs) = &mut app.view else {
        return;
    };
    let ws = tabs.current_mut();
    let show_working_tree_panel = ws.selected_working_tree;

    // If a diff is computing and none is ready yet, render a slim
    // "Computing diff…" panel instead of silently doing nothing. This
    // gives the user feedback when clicking Linux-kernel merge commits
    // that take several seconds.
    // Unified panel — we keep the SAME `SidePanel::right` id for both the
    // "Computing…" state and the loaded-diff state. Previously the loading
    // panel had its own id, so egui treated it as a distinct widget and
    // reset the panel width on every transition. Clicking a commit made
    // the diff panel snap-shake each time the diff hop-scotched between
    // these two ids.
    let diff = ws.current_diff.clone();
    // Snapshot combined-diff metadata too — the panel renders the
    // synthetic "N commits combined" banner in place of the usual
    // single-commit summary when this is set.
    let combined_diff_sources = ws.combined_diff_source.clone();
    // And the active focus-file filter, if any. Presence of a focus
    // path decorates the banner with "Focused on: <path>" plus a
    // chip to clear the filter and restore the full combined diff.
    let combined_diff_focus = ws.combined_diff_focus_path.clone();
    if !show_working_tree_panel && diff.is_none() && ws.diff_task.is_none() {
        // Nothing to show and nothing computing — don't render the panel.
        return;
    }

    let mut close = false;
    let mut clear_focus = false;

    egui::SidePanel::right("diff_panel")
        .resizable(true)
        .min_width(PANEL_MIN_WIDTH)
        .default_width(PANEL_DEFAULT_WIDTH)
        .show(ctx, |ui| {
            if show_working_tree_panel {
                render_working_tree_panel(ui, ws);
                return;
            }
            // Computing state: show spinner inside the SAME panel so the
            // panel width + neighbours don't twitch when the diff lands.
            if diff.is_none() {
                if let Some(task) = ws.diff_task.as_ref() {
                    let elapsed = task.started_at.elapsed().as_secs();
                    let oid_short = {
                        let s = task.oid.to_string();
                        s[..7.min(s.len())].to_string()
                    };
                    ui.add_space(24.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("Computing diff for {oid_short}…"));
                    });
                    ui.weak(format!("{elapsed}s"));
                    ui.add_space(8.0);
                    ui.weak(
                        "Large merge commits (kernel-scale) can take several \
                         seconds. Rename detection is skipped above 800 changed \
                         files so the wait stays bounded.",
                    );
                    ctx.request_repaint_after(std::time::Duration::from_millis(250));
                }
                return;
            }
            let diff = diff.as_ref().unwrap();
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.heading("Commit");
                    ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                        let close_button =
                            egui::Button::new(RichText::new("x").strong().size(16.0).monospace())
                                .frame(true);
                        if ui
                            .add_sized([30.0, 30.0], close_button)
                            .on_hover_text("Close diff")
                            .clicked()
                        {
                            close = true;
                        }
                    });
                });
                if let Some(sources) = combined_diff_sources {
                    if render_combined_diff_banner(
                        ui,
                        &sources,
                        combined_diff_focus.as_deref(),
                    ) {
                        clear_focus = true;
                    }
                } else {
                    render_commit_summary(ui, &diff);
                }
                ui.separator();

                // File list header: count + flat/tree toggle.
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("Files ({})", diff.files.len())).strong());
                    ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                        let btn_size = egui::vec2(26.0, 22.0);
                        if ui
                            .add_sized(
                                btn_size,
                                egui::SelectableLabel::new(ws.file_list_tree, "🌲"),
                            )
                            .on_hover_text("Tree view — group files by directory")
                            .clicked()
                        {
                            ws.file_list_tree = true;
                        }
                        if ui
                            .add_sized(
                                btn_size,
                                egui::SelectableLabel::new(!ws.file_list_tree, "≡"),
                            )
                            .on_hover_text("Flat list — files sorted by path")
                            .clicked()
                        {
                            ws.file_list_tree = false;
                        }
                    });
                });

                if ws.file_list_tree {
                    render_file_tree(ui, ws, &diff);
                } else {
                    render_file_flat(ui, ws, &diff);
                }
            });
        });

    if close {
        ws.selected_commit = None;
        ws.current_diff = None;
        ws.selected_file_idx = None;
        ws.selected_file_view = SelectedFileView::Diff;
        ws.set_image_cache(None);
        // Preserve the graph selection visually even after closing the diff:
        // we only clear the diff pane, not the row highlight.
    }
    // Restore the unfiltered combined diff in-place. We intentionally
    // keep `combined_diff_full` populated — the user can re-open the
    // picker and pick a different path without recomputing the
    // cherry-pick chain.
    if clear_focus && ws.combined_diff_focus_path.is_some() {
        ws.combined_diff_focus_path = None;
        if let Some(full) = ws.combined_diff_full.clone() {
            ws.current_diff = Some(full);
        }
        ws.selected_file_idx = None;
        ws.set_image_cache(None);
    }
}

fn render_working_tree_panel(ui: &mut egui::Ui, ws: &mut WorkspaceState) {
    let working_error = ws
        .repo_ui_cache
        .as_ref()
        .and_then(|c| c.working_error.clone());
    let entries: Vec<StatusEntry> = ws
        .repo_ui_cache
        .as_ref()
        .and_then(|c| c.working.clone())
        .unwrap_or_default();

    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.heading("Changes");
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                let close_button =
                    egui::Button::new(RichText::new("x").strong().size(16.0).monospace())
                        .frame(true);
                if ui
                    .add_sized([30.0, 30.0], close_button)
                    .on_hover_text("Close changes panel")
                    .clicked()
                {
                    ws.selected_working_tree = false;
                    ws.selected_working_file = None;
                    ws.set_image_cache(None);
                    ws.working_file_diff = None;
                }
            });
        });
        if let Some(err) = working_error {
            ui.colored_label(egui::Color32::from_rgb(230, 180, 90), err);
            ui.add_space(6.0);
        }
        render_working_tree_summary(ui, &entries);
        ui.separator();

        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Files ({})", entries.len())).strong());
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                let btn_size = egui::vec2(26.0, 22.0);
                if ui
                    .add_sized(
                        btn_size,
                        egui::SelectableLabel::new(ws.file_list_tree, "🌲"),
                    )
                    .on_hover_text("Tree view — group files by directory")
                    .clicked()
                {
                    ws.file_list_tree = true;
                }
                if ui
                    .add_sized(
                        btn_size,
                        egui::SelectableLabel::new(!ws.file_list_tree, "≡"),
                    )
                    .on_hover_text("Flat list — files sorted by path")
                    .clicked()
                {
                    ws.file_list_tree = false;
                }
            });
        });

        if ws.file_list_tree {
            render_working_tree_tree(ui, ws, &entries);
        } else {
            render_working_tree_flat(ui, ws, &entries);
        }
    });
}

fn render_working_tree_summary(ui: &mut egui::Ui, entries: &[StatusEntry]) {
    let staged = entries.iter().filter(|e| e.staged).count();
    let unstaged = entries.iter().filter(|e| e.unstaged).count();
    let untracked = entries
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::Untracked))
        .count();
    let conflicted = entries.iter().filter(|e| e.conflicted).count();

    let mut parts = Vec::new();
    if conflicted > 0 {
        parts.push(format!("{conflicted} conflicted"));
    }
    if staged > 0 {
        parts.push(format!("{staged} staged"));
    }
    if unstaged > 0 {
        parts.push(format!("{unstaged} unstaged"));
    }
    if untracked > 0 {
        parts.push(format!("{untracked} untracked"));
    }

    if parts.is_empty() {
        ui.weak("No uncommitted changes.");
    } else {
        ui.weak(parts.join(" · "));
    }
}

fn render_working_tree_flat(ui: &mut egui::Ui, ws: &mut WorkspaceState, entries: &[StatusEntry]) {
    let total_files = entries.len();
    ScrollArea::vertical()
        .id_salt("working_tree_files")
        .auto_shrink([false, false])
        .show_rows(ui, FILE_ROW_HEIGHT, total_files, |ui, range| {
            let row_width = ui.available_width();
            for i in range {
                let entry = &entries[i];
                let selected = ws
                    .selected_working_file
                    .as_ref()
                    .map(|path| path == &entry.path)
                    .unwrap_or(false);
                let color = working_tree_status_color(entry);
                let (rect, resp) = ui.allocate_exact_size(
                    egui::vec2(row_width, FILE_ROW_HEIGHT),
                    egui::Sense::click(),
                );

                if selected {
                    ui.painter().rect_filled(
                        rect,
                        0.0,
                        ui.visuals().selection.bg_fill.gamma_multiply(0.4),
                    );
                } else if resp.hovered() {
                    ui.painter()
                        .rect_filled(rect, 0.0, ui.visuals().faint_bg_color);
                }

                let mut child = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                child.label(
                    RichText::new(entry.kind.glyph())
                        .color(color)
                        .monospace()
                        .strong(),
                );
                child.add_space(4.0);
                child.add(
                    egui::Label::new(RichText::new(entry.path.display().to_string()).monospace())
                        .truncate()
                        .selectable(false),
                );

                let galley = ui.painter().layout_no_wrap(
                    working_tree_stats_str(entry),
                    FontId::monospace(11.0),
                    ui.visuals().weak_text_color(),
                );
                ui.painter().galley(
                    egui::pos2(
                        rect.right() - galley.size().x - 4.0,
                        rect.center().y - galley.size().y * 0.5,
                    ),
                    galley,
                    ui.visuals().weak_text_color(),
                );

                if resp.clicked() {
                    if selected {
                        ws.selected_working_file = None;
                        ws.working_file_diff = None;
                    } else {
                        ws.selected_working_file = Some(entry.path.clone());
                        ws.working_file_diff =
                            crate::git::diff_text_for_working_entry(ws.repo.path(), entry).ok();
                    }
                    ws.selected_file_view = SelectedFileView::Diff;
                    ws.set_image_cache(None);
                }

                resp.on_hover_text(entry.path.display().to_string());
            }
        });
}

fn render_working_tree_tree(ui: &mut egui::Ui, ws: &mut WorkspaceState, entries: &[StatusEntry]) {
    let mut dirs: std::collections::BTreeMap<String, Vec<&StatusEntry>> =
        std::collections::BTreeMap::new();
    for entry in entries {
        let path = entry.path.display().to_string();
        let (dir, _name) = match path.rfind('/') {
            Some(pos) => (&path[..pos], &path[pos + 1..]),
            None => (".", path.as_str()),
        };
        dirs.entry(dir.to_string()).or_default().push(entry);
    }

    ScrollArea::vertical()
        .id_salt("working_tree_files_tree")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (dir, files) in &dirs {
                let dir_label = if dir == "." {
                    "(root)".to_string()
                } else {
                    format!("📁 {dir}/")
                };
                egui::CollapsingHeader::new(
                    RichText::new(format!("{dir_label}  ({})", files.len())).weak(),
                )
                .default_open(true)
                .show(ui, |ui| {
                    for entry in files {
                        let selected = ws
                            .selected_working_file
                            .as_ref()
                            .map(|path| path == &entry.path)
                            .unwrap_or(false);
                        let display = entry.path.display().to_string();
                        let file_name = display.rsplit('/').next().unwrap_or(&display);
                        let color = working_tree_status_color(entry);
                        let row_w = ui.available_width();
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(row_w, FILE_ROW_HEIGHT),
                            egui::Sense::click(),
                        );
                        if selected {
                            ui.painter().rect_filled(
                                rect,
                                0.0,
                                ui.visuals().selection.bg_fill.gamma_multiply(0.4),
                            );
                        } else if resp.hovered() {
                            ui.painter()
                                .rect_filled(rect, 0.0, ui.visuals().faint_bg_color);
                        }

                        let mut child = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(rect)
                                .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        );
                        child.label(
                            RichText::new(entry.kind.glyph())
                                .color(color)
                                .monospace()
                                .strong(),
                        );
                        child.add_space(4.0);
                        child.add(
                            egui::Label::new(RichText::new(file_name).monospace())
                                .truncate()
                                .selectable(false),
                        );

                        let galley = ui.painter().layout_no_wrap(
                            working_tree_stats_str(entry),
                            FontId::monospace(11.0),
                            ui.visuals().weak_text_color(),
                        );
                        ui.painter().galley(
                            egui::pos2(
                                rect.right() - galley.size().x - 4.0,
                                rect.center().y - galley.size().y * 0.5,
                            ),
                            galley,
                            ui.visuals().weak_text_color(),
                        );

                        if resp.clicked() {
                            if selected {
                                ws.selected_working_file = None;
                                ws.working_file_diff = None;
                            } else {
                                ws.selected_working_file = Some(entry.path.clone());
                                ws.working_file_diff =
                                    crate::git::diff_text_for_working_entry(ws.repo.path(), entry)
                                        .ok();
                            }
                            ws.selected_file_view = SelectedFileView::Diff;
                            ws.set_image_cache(None);
                        }
                    }
                });
            }
        });
}

fn working_tree_status_color(entry: &StatusEntry) -> Color32 {
    match entry.kind {
        EntryKind::New | EntryKind::Untracked => Color32::from_rgb(90, 180, 120),
        EntryKind::Modified => Color32::from_rgb(220, 190, 90),
        EntryKind::Deleted => Color32::from_rgb(220, 100, 100),
        EntryKind::Renamed => Color32::from_rgb(150, 150, 220),
        EntryKind::Typechange => Color32::from_rgb(200, 120, 200),
        EntryKind::Conflicted => Color32::from_rgb(255, 80, 80),
    }
}

fn working_tree_stats_str(entry: &StatusEntry) -> String {
    if entry.conflicted {
        "conflicted".to_string()
    } else if matches!(entry.kind, EntryKind::Untracked) {
        "[new]".to_string()
    } else if entry.staged && entry.unstaged {
        "[staged+wt]".to_string()
    } else if entry.staged {
        "[staged]".to_string()
    } else {
        "[wt]".to_string()
    }
}

pub(crate) fn has_selected_file(ws: &WorkspaceState) -> bool {
    // Check if a commit file is selected
    let commit_file_selected = ws
        .current_diff
        .as_ref()
        .and_then(|diff| ws.selected_file_idx.and_then(|idx| diff.files.get(idx)))
        .is_some();
    // Or a working tree file is selected
    let working_file_selected = ws.selected_working_tree && ws.selected_working_file.is_some();
    commit_file_selected || working_file_selected
}

pub(crate) fn show_selected_file_center(ui: &mut egui::Ui, ws: &mut WorkspaceState) {
    // Working Tree file selected: render it inline
    if ws.selected_working_tree {
        if let Some(path) = ws.selected_working_file.clone() {
            render_working_file_center(ui, ws, &path);
            return;
        }
    }

    // Commit file selected: existing logic
    let Some(diff) = ws.current_diff.clone() else {
        ui.vertical_centered(|ui| ui.weak("No commit selected."));
        return;
    };
    let Some(selected_idx) = ws.selected_file_idx else {
        ui.vertical_centered(|ui| {
            ui.weak("Select a file from the right panel to open its diff or file view.")
        });
        return;
    };
    let Some(file) = diff.files.get(selected_idx).cloned() else {
        ws.selected_file_idx = None;
        ui.vertical_centered(|ui| {
            ui.weak("Select a file from the right panel to open its diff or file view.")
        });
        return;
    };
    let image_cache = selected_image_cache(ws, &file);

    ui.horizontal(|ui| {
        if ui.button("← History").clicked() {
            ws.selected_file_idx = None;
            ws.set_image_cache(None);
        }
        ui.separator();
        ui.label(RichText::new(file.display_path()).strong());
        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .selectable_label(ws.selected_file_view == SelectedFileView::File, "File View")
                .clicked()
            {
                ws.selected_file_view = SelectedFileView::File;
            }
            if ui
                .selectable_label(ws.selected_file_view == SelectedFileView::Diff, "Diff View")
                .clicked()
            {
                ws.selected_file_view = SelectedFileView::Diff;
            }
        });
    });
    ui.small(&diff.title);
    ui.separator();

    // Each render branch manages its own scroll policy: the text-diff
    // and snapshot paths use a *vertical* virtualized scroll (show_rows)
    // so files with tens of thousands of lines don't lay out off-screen
    // rows every frame. The image / binary / too-large branches use a
    // plain `ScrollArea::both` since their content is small.
    match ws.selected_file_view {
        SelectedFileView::Diff => render_file_detail(ui, &file, image_cache.as_ref()),
        SelectedFileView::File => {
            render_file_snapshot(ui, ws, &file, image_cache.as_ref());
        }
    }
}

/// Render a working tree file (staged or unstaged) in the center pane.
fn render_working_file_center(ui: &mut egui::Ui, ws: &mut WorkspaceState, path: &std::path::Path) {
    let Some(entry) = selected_working_entry(ws, path) else {
        ws.selected_working_file = None;
        ws.working_file_diff = None;
        ui.vertical_centered(|ui| {
            ui.weak("Select a file from the right panel to open its diff or file view.")
        });
        return;
    };
    let path_str = path.display().to_string();

    ui.horizontal(|ui| {
        if ui.button("← Changes").clicked() {
            ws.selected_working_file = None;
            ws.set_image_cache(None);
            ws.working_file_diff = None;
        }
        ui.separator();
        ui.label(RichText::new(&path_str).strong());
        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .selectable_label(ws.selected_file_view == SelectedFileView::File, "File View")
                .clicked()
            {
                ws.selected_file_view = SelectedFileView::File;
            }
            if ui
                .selectable_label(ws.selected_file_view == SelectedFileView::Diff, "Diff View")
                .clicked()
            {
                ws.selected_file_view = SelectedFileView::Diff;
            }
        });
    });
    ui.small(format!("Changes · {}", working_tree_stats_str(&entry)));
    ui.separator();

    if ws.working_file_diff.is_none() {
        ws.working_file_diff = crate::git::diff_text_for_working_entry(ws.repo.path(), &entry).ok();
    }

    let working_file = ws
        .working_file_diff
        .as_deref()
        .map(|text| crate::git::file_diff_for_working_entry(&entry, text));
    let image_cache = working_file
        .as_ref()
        .and_then(|file| working_tree_image_cache(ws.repo.path(), file));

    match ws.selected_file_view {
        SelectedFileView::Diff => {
            if let Some(file) = working_file.as_ref() {
                render_file_detail(ui, file, image_cache.as_ref());
            } else {
                ui.weak("Could not compute diff for this file.");
            }
        }
        SelectedFileView::File => {
            if let Some(file) = working_file.as_ref() {
                render_working_file_snapshot(ui, ws.repo.path(), file, image_cache.as_ref());
            } else {
                ui.weak("Could not load file contents for this working tree file.");
            }
        }
    }
}

fn selected_working_entry(
    ws: &WorkspaceState,
    path: &std::path::Path,
) -> Option<crate::git::StatusEntry> {
    ws.repo_ui_cache
        .as_ref()
        .and_then(|cache| cache.working.as_ref())
        .and_then(|entries| entries.iter().find(|entry| entry.path == path))
        .cloned()
}

fn working_tree_image_cache(
    repo_path: &std::path::Path,
    file: &FileDiff,
) -> Option<SelectedImageCache> {
    let FileKind::Image { ext } = &file.kind else {
        return None;
    };

    Some(SelectedImageCache {
        old_oid: None,
        new_oid: None,
        old_bytes: file
            .old_path
            .as_ref()
            .and_then(|path| load_git_snapshot_bytes(repo_path, path)),
        new_bytes: file
            .new_path
            .as_ref()
            .and_then(|path| load_working_tree_bytes(repo_path, path))
            .or_else(|| {
                file.new_path
                    .as_ref()
                    .and_then(|path| load_git_snapshot_bytes(repo_path, path))
            }),
        ext: ext.clone(),
    })
}

fn render_working_file_snapshot(
    ui: &mut egui::Ui,
    repo_path: &std::path::Path,
    file: &FileDiff,
    image_cache: Option<&SelectedImageCache>,
) {
    match &file.kind {
        FileKind::Text { .. } => {
            let Some(text) = load_working_tree_snapshot_text(repo_path, file) else {
                ui.weak("Could not load file contents for this working tree file.");
                return;
            };
            let bounds = compute_line_bounds(&text);
            if bounds.is_empty() {
                ui.weak("(empty file)");
                return;
            }
            render_text_snapshot(ui, &file.display_path(), &text, &bounds);
        }
        FileKind::Image { ext } => {
            // Working-tree case: the "current" bytes may come from disk
            // (unstaged edit) — we don't always have an oid. The preview
            // pipeline accepts anonymous blobs by caching against a
            // pointer hash, so the thumbnail cache still dedupes across
            // frames even without a content address.
            let bytes = image_cache
                .and_then(|cache| cache.new_bytes.clone().or(cache.old_bytes.clone()));
            let caption = if file.new_path.is_some() {
                "Current working tree file"
            } else {
                "Deleted file (last committed contents)"
            };
            render_single_image_preview(
                ui,
                "working_snapshot_image",
                caption,
                bytes,
                None,
                ext,
                file.new_size.max(file.old_size),
            );
        }
        FileKind::Binary => {
            ui.weak("Binary working tree file snapshot is not shown inline.");
        }
        FileKind::TooLarge => {
            ui.weak(format!(
                "File snapshot is too large to render inline (>{} MB).",
                crate::git::diff::MAX_BLOB_BYTES / (1024 * 1024)
            ));
        }
    }
}

fn load_working_tree_snapshot_text(repo_path: &std::path::Path, file: &FileDiff) -> Option<String> {
    file.new_path
        .as_ref()
        .and_then(|path| load_working_tree_text(repo_path, path))
        .or_else(|| {
            file.old_path
                .as_ref()
                .and_then(|path| load_git_snapshot_text(repo_path, path))
        })
}

fn load_working_tree_text(repo_path: &std::path::Path, path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(repo_path.join(path)).ok()
}

fn load_working_tree_bytes(
    repo_path: &std::path::Path,
    path: &std::path::Path,
) -> Option<Arc<[u8]>> {
    let bytes = std::fs::read(repo_path.join(path)).ok()?;
    if bytes.len() > crate::git::diff::MAX_BLOB_BYTES {
        return None;
    }
    Some(Arc::from(bytes))
}

fn load_git_snapshot_text(repo_path: &std::path::Path, path: &std::path::Path) -> Option<String> {
    let bytes = load_git_snapshot_bytes(repo_path, path)?;
    std::str::from_utf8(&bytes).ok().map(str::to_owned)
}

fn load_git_snapshot_bytes(
    repo_path: &std::path::Path,
    path: &std::path::Path,
) -> Option<Arc<[u8]>> {
    let path_str = path.display().to_string();
    for spec in [format!(":{path_str}"), format!("HEAD:{path_str}")] {
        let Ok(out) = crate::git::cli::run(repo_path, ["show", &spec]) else {
            continue;
        };
        if out.stdout.len() > crate::git::diff::MAX_BLOB_BYTES {
            return None;
        }
        return Some(Arc::from(out.stdout));
    }
    None
}

fn path_looks_like_image(path: &std::path::Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("png")
            | Some("jpg")
            | Some("jpeg")
            | Some("gif")
            | Some("webp")
            | Some("bmp")
            | Some("ico")
            | Some("tiff")
            | Some("tif")
    )
}

/// Top-of-panel banner for a synthetic combined diff (produced by
/// `basket_ops::compute_combined_diff`). Shows the cumulative count
/// and the topo-sorted SHAs so the user can tell "which commits got
/// collapsed into this diff" at a glance.
///
/// When `focus_path` is set, the banner additionally renders a
/// "Focused on: <path>" row with a "Clear filter" chip; returning
/// `true` signals the caller to drop the focus filter this frame.
/// We funnel the click through a return value rather than mutating
/// `WorkspaceState` here because this function is deliberately
/// UI-only — keeping side-effects at the call site mirrors the
/// `close = true` pattern already used for the panel's x-button.
fn render_combined_diff_banner(
    ui: &mut egui::Ui,
    sources: &[gix::ObjectId],
    focus_path: Option<&str>,
) -> bool {
    let mut clear_focus = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!("✦ Combined diff of {} commits", sources.len()))
                .strong()
                .color(Color32::from_rgb(210, 180, 100)),
        );
    });
    if !sources.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.weak("order:");
            for (idx, oid) in sources.iter().enumerate() {
                if idx > 0 {
                    ui.weak("→");
                }
                ui.add(egui::Label::new(
                    RichText::new(short_sha(oid)).monospace(),
                ))
                .on_hover_text(oid.to_string());
            }
        });
    }
    ui.weak("Cherry-pick apply order (oldest → newest). Read-only synthetic diff.");
    if let Some(path) = focus_path {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new("Focused on:")
                    .color(Color32::from_rgb(210, 180, 100))
                    .strong(),
            );
            ui.add(egui::Label::new(RichText::new(path).monospace()))
                .on_hover_text(path);
            if ui
                .small_button("✕ Clear filter")
                .on_hover_text(
                    "Show all files in the combined diff again. The file \
                     picker can be re-opened from the basket bar.",
                )
                .clicked()
            {
                clear_focus = true;
            }
        });
    }
    clear_focus
}

fn render_commit_summary(ui: &mut egui::Ui, diff: &RepoDiff) {
    // Top header row: short commit hash + parent hashes. Matches the
    // `commit: 9d3b…  parent: 3046af` shape of most other Git GUIs —
    // puts identity information above the message so users can
    // cross-reference with `git log` or `git show`.
    if let Some(oid) = diff.commit_oid.as_ref() {
        ui.horizontal_wrapped(|ui| {
            ui.weak("commit:");
            ui.add(
                egui::Label::new(RichText::new(short_sha(oid)).monospace().strong())
                    .sense(egui::Sense::click()),
            )
            .on_hover_text(oid.to_string());

            if !diff.commit_parent_oids.is_empty() {
                ui.add_space(8.0);
                ui.weak(if diff.commit_parent_oids.len() == 1 {
                    "parent:"
                } else {
                    "parents:"
                });
                for parent in &diff.commit_parent_oids {
                    ui.add(egui::Label::new(
                        RichText::new(short_sha(parent)).monospace(),
                    ))
                    .on_hover_text(parent.to_string());
                }
            }
        });
    }

    let message = diff
        .commit_message
        .as_deref()
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .unwrap_or("(no commit message)");

    // Constrain the message block: long commit messages (paragraph-style
    // merge notes, squashed histories with release notes) used to push
    // the file list off-screen. We give the message a fixed 160 px tall
    // box with its own ScrollArea so it stays bounded and the rest of
    // the panel layout is stable regardless of message length.
    const MESSAGE_BOX_MAX_HEIGHT: f32 = 160.0;
    egui::ScrollArea::vertical()
        .id_salt("commit_message_scroll")
        .max_height(MESSAGE_BOX_MAX_HEIGHT)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            // `label_wrap` equivalent: we want the subject line bold and
            // the body wrapping to panel width. Single `Label` handles
            // both since the message's own newlines are respected.
            ui.add(egui::Label::new(RichText::new(message).strong()).wrap());
        });

    // Author + time row: `<name>  authored <absolute> (<relative>)`.
    // Absolute time anchors the user in real history; relative time
    // ("2h ago") gives the "how recent" glance. Both together avoids
    // the "yesterday or last month?" re-check that a relative-only
    // label forces.
    let author = diff
        .commit_author
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty());
    if author.is_some() || diff.commit_author_time.is_some() {
        ui.horizontal_wrapped(|ui| {
            if let Some(name) = author {
                let hover = diff
                    .commit_author_email
                    .as_deref()
                    .map(str::trim)
                    .filter(|e| !e.is_empty())
                    .map(|e| format!("{name} <{e}>"))
                    .unwrap_or_else(|| name.to_string());
                ui.add(egui::Label::new(RichText::new(name).weak()))
                    .on_hover_text(hover);
            }
            if let Some(ts) = diff.commit_author_time {
                if author.is_some() {
                    ui.add_space(6.0);
                    ui.weak("•");
                    ui.add_space(6.0);
                }
                ui.weak(format!("authored {}", format_commit_time(ts)));
                ui.add_space(6.0);
                ui.weak(format!("({})", relative_time(ts)));
            }
        });
    }
}

fn short_sha(oid: &gix::ObjectId) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}

/// Absolute timestamp formatter. Shows UTC to avoid depending on
/// chrono / libc `localtime_r` for one label; the relative time
/// ("3h ago") next to it gives the at-a-glance sense that a local
/// conversion would add. Users who need a precise local reading can
/// cross-check via their terminal.
fn format_commit_time(unix: i64) -> String {
    if unix <= 0 {
        return "unknown".to_string();
    }
    let days = unix.div_euclid(86_400);
    let tod = unix.rem_euclid(86_400) as u32;
    let (year, month, day) = civil_from_days(days);
    let hour = tod / 3600;
    let minute = (tod % 3600) / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

/// Days since Unix epoch → civil (year, month, day). Same Howard
/// Hinnant algorithm used in `blame.rs::civil_from_days`; duplicated
/// here rather than re-exported to keep `blame.rs` private.
fn civil_from_days(mut z: i64) -> (i32, u32, u32) {
    z += 719468;
    let era = if z >= 0 { z / 146097 } else { (z - 146096) / 146097 };
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Coarse "N unit ago" label. Seconds granularity at the minute mark
/// would look fussy next to the absolute YYYY-MM-DD; months and years
/// are rounded because nobody needs day-precision at that scale.
fn relative_time(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let diff = (now - ts).max(0);
    match diff {
        d if d < 60 => "just now".into(),
        d if d < 3600 => format!("{}m ago", d / 60),
        d if d < 86_400 => format!("{}h ago", d / 3600),
        d if d < 2_592_000 => format!("{}d ago", d / 86_400),
        d if d < 31_536_000 => format!("{}mo ago", d / 2_592_000),
        d => format!("{}y ago", d / 31_536_000),
    }
}

/// Flat file list — virtualized, left-aligned glyph + path, right-aligned stats.
/// Full-width click target so selection highlight spans the whole row.
///
/// Row height is uniform across the list: if any file in the diff is an
/// image/PSD asset we bump *every* row to [`FILE_ROW_HEIGHT_WITH_THUMB`]
/// so the virtualized renderer (`show_rows`) can keep indexing by
/// `row_idx * row_height`. Per-row adaptive heights would defeat
/// virtualization on thousand-file diffs (Linux kernel merge commits)
/// which is where the file list's scroll performance actually matters.
fn render_file_flat(ui: &mut egui::Ui, ws: &mut WorkspaceState, diff: &RepoDiff) {
    let total_files = diff.files.len();
    let has_any_thumb = diff
        .files
        .iter()
        .any(|f| matches!(f.kind, FileKind::Image { .. }));
    let row_height = if has_any_thumb {
        FILE_ROW_HEIGHT_WITH_THUMB
    } else {
        FILE_ROW_HEIGHT
    };
    let repo = ws.repo.gix().clone();
    ScrollArea::vertical()
        .id_salt("diff_files_flat")
        .auto_shrink([false, false])
        .show_rows(ui, row_height, total_files, |ui, range| {
            let row_width = ui.available_width();
            for i in range {
                let file = &diff.files[i];
                let selected = ws.selected_file_idx == Some(i);
                let color = status_color(file.status);
                let (rect, resp) = ui.allocate_exact_size(
                    egui::vec2(row_width, row_height),
                    egui::Sense::click(),
                );
                if selected {
                    ui.painter().rect_filled(
                        rect,
                        0.0,
                        ui.visuals().selection.bg_fill.gamma_multiply(0.4),
                    );
                } else if resp.hovered() {
                    ui.painter()
                        .rect_filled(rect, 0.0, ui.visuals().faint_bg_color);
                }
                let mut child = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                child.label(
                    RichText::new(file.status.glyph())
                        .color(color)
                        .monospace()
                        .strong(),
                );
                child.add_space(4.0);
                if has_any_thumb {
                    paint_inline_thumbnail(&mut child, &repo, file);
                    child.add_space(6.0);
                }
                child.add(
                    egui::Label::new(RichText::new(file.display_path()).monospace())
                        .truncate()
                        .selectable(false),
                );
                let stats_text = file_stats_str(file);
                let stats_galley = ui.painter().layout_no_wrap(
                    stats_text,
                    egui::FontId::monospace(11.0),
                    ui.visuals().weak_text_color(),
                );
                let stats_x = rect.right() - stats_galley.size().x - 4.0;
                let stats_y = rect.center().y - stats_galley.size().y * 0.5;
                ui.painter().galley(
                    egui::pos2(stats_x, stats_y),
                    stats_galley,
                    ui.visuals().weak_text_color(),
                );
                if resp.clicked() {
                    ws.selected_file_idx = if selected { None } else { Some(i) };
                    ws.selected_file_view = SelectedFileView::Diff;
                    ws.set_image_cache(None);
                }
            }
        });
}

/// Tree view — files grouped by directory with collapsible headers.
///
/// Like [`render_file_flat`] we pick a single row height based on
/// whether *any* file in the diff is an image; keeps tree-leaf rows
/// visually aligned so path letters sit on the same baseline whether
/// a row has a thumbnail or not.
fn render_file_tree(ui: &mut egui::Ui, ws: &mut WorkspaceState, diff: &RepoDiff) {
    let mut dirs: std::collections::BTreeMap<String, Vec<(usize, &FileDiff)>> =
        std::collections::BTreeMap::new();
    for (i, file) in diff.files.iter().enumerate() {
        let path = file.display_path();
        let (dir, _name) = match path.rfind('/') {
            Some(pos) => (&path[..pos], &path[pos + 1..]),
            None => (".", path.as_str()),
        };
        dirs.entry(dir.to_string()).or_default().push((i, file));
    }

    let has_any_thumb = diff
        .files
        .iter()
        .any(|f| matches!(f.kind, FileKind::Image { .. }));
    let row_height = if has_any_thumb {
        FILE_ROW_HEIGHT_WITH_THUMB
    } else {
        FILE_ROW_HEIGHT
    };
    let repo = ws.repo.gix().clone();

    ScrollArea::vertical()
        .id_salt("diff_files_tree")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (dir, files) in &dirs {
                let dir_label = if dir == "." {
                    "(root)".to_string()
                } else {
                    format!("📁 {dir}/")
                };
                egui::CollapsingHeader::new(
                    RichText::new(format!("{dir_label}  ({})", files.len())).weak(),
                )
                .default_open(true)
                .show(ui, |ui| {
                    for &(i, file) in files {
                        let selected = ws.selected_file_idx == Some(i);
                        let display = file.display_path();
                        let file_name = display.rsplit('/').next().unwrap_or(&display);
                        let color = status_color(file.status);
                        let row_w = ui.available_width();
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(row_w, row_height),
                            egui::Sense::click(),
                        );
                        if selected {
                            ui.painter().rect_filled(
                                rect,
                                0.0,
                                ui.visuals().selection.bg_fill.gamma_multiply(0.4),
                            );
                        } else if resp.hovered() {
                            ui.painter()
                                .rect_filled(rect, 0.0, ui.visuals().faint_bg_color);
                        }
                        let mut child = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(rect)
                                .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        );
                        child.label(
                            RichText::new(file.status.glyph())
                                .color(color)
                                .monospace()
                                .strong(),
                        );
                        child.add_space(4.0);
                        if has_any_thumb {
                            paint_inline_thumbnail(&mut child, &repo, file);
                            child.add_space(6.0);
                        }
                        child.add(
                            egui::Label::new(RichText::new(file_name).monospace())
                                .truncate()
                                .selectable(false),
                        );
                        let stats = file_stats_str(file);
                        let galley = ui.painter().layout_no_wrap(
                            stats,
                            egui::FontId::monospace(11.0),
                            ui.visuals().weak_text_color(),
                        );
                        ui.painter().galley(
                            egui::pos2(
                                rect.right() - galley.size().x - 4.0,
                                rect.center().y - galley.size().y * 0.5,
                            ),
                            galley,
                            ui.visuals().weak_text_color(),
                        );
                        if resp.clicked() {
                            ws.selected_file_idx = if selected { None } else { Some(i) };
                            ws.selected_file_view = SelectedFileView::Diff;
                            ws.set_image_cache(None);
                        }
                    }
                });
            }
        });
}

/// Draw the inline-thumbnail slot for a single file row, reserving the
/// same layout footprint whether or not this specific file is an image.
/// Non-image rows get a transparent spacer so the path text column is
/// still aligned with the image rows above/below.
///
/// Behavior by state:
///   * not an image → blank spacer of equal size
///   * `Ready`      → the decoded texture, rendered fit-to-square
///   * `Pending`    → a faint placeholder square so row width doesn't
///                     reflow when the decode lands mid-frame
///   * `Unsupported`/`TooLarge`/`Failed` → blank spacer; the typed
///                     placeholder / error lives in the detail pane
///                     where there's room to explain, not the 22px row
fn paint_inline_thumbnail(ui: &mut egui::Ui, repo: &gix::Repository, file: &FileDiff) {
    let size = egui::vec2(THUMB_DRAW_SIZE, THUMB_DRAW_SIZE);
    let FileKind::Image { ext } = &file.kind else {
        ui.allocate_exact_size(size, egui::Sense::hover());
        return;
    };
    // Prefer `new_oid` (the state after the commit) so the thumbnail
    // matches the "after" image in the diff pane. For pure deletions
    // we still show the last-committed image from `old_oid` — the row
    // is explicitly labelled with a 'D' glyph so there's no ambiguity.
    let oid = match file.new_oid.or(file.old_oid) {
        Some(o) => o,
        None => {
            ui.allocate_exact_size(size, egui::Sense::hover());
            return;
        }
    };
    let key = crate::ui::file_preview::PreviewKey {
        identity: crate::ui::file_preview::PreviewIdentity::Blob(oid),
        mode: crate::ui::file_preview::PreviewMode::Thumb,
    };
    let manager = crate::ui::file_preview::PreviewManager::global();
    // Peek before reading the blob: `load_blob_bytes` copies into an
    // `Arc<[u8]>` each call, and the row renders once per frame for
    // every visible file. Cache-hit fast-path means we never touch the
    // object DB beyond the first miss.
    let state = match manager.peek(&key) {
        Some(s) => s,
        None => {
            if let Some(bytes) = crate::git::diff::load_blob_bytes(repo, Some(oid)) {
                manager.request_blob(
                    oid,
                    bytes,
                    ext,
                    crate::ui::file_preview::PreviewMode::Thumb,
                )
            } else {
                // Missing blob (shouldn't happen on a well-formed diff):
                // render a blank spacer, don't poison the cache.
                ui.allocate_exact_size(size, egui::Sense::hover());
                return;
            }
        }
    };
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    match state {
        crate::ui::file_preview::PreviewState::Ready(_) => {
            if let Some(tex) = manager.texture_for(ui.ctx(), &key, &format!("thumb-{oid}")) {
                let tex_size = tex.size_vec2();
                // Fit-to-square while preserving aspect ratio.
                let scale = (rect.width() / tex_size.x).min(rect.height() / tex_size.y);
                let draw_size = tex_size * scale;
                let draw_rect = egui::Rect::from_center_size(rect.center(), draw_size);
                egui::Image::new(&tex).paint_at(ui, draw_rect);
            } else {
                // Transient (texture promotion race on the exact frame
                // the Ready state landed): treat like Pending for one
                // frame, then the next frame has the handle cached.
                ui.painter().rect_filled(
                    rect,
                    2.0,
                    ui.visuals().faint_bg_color.gamma_multiply(0.6),
                );
            }
        }
        crate::ui::file_preview::PreviewState::Pending => {
            ui.painter()
                .rect_filled(rect, 2.0, ui.visuals().faint_bg_color);
            // Schedule a repaint so we don't have to wait for user
            // input to re-check the pending state. 50 ms matches the
            // upper end of a "feels instant" perceptual window.
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }
        _ => {
            // Failed / TooLarge / Unsupported: leave the slot blank.
            // The full preview pane will surface the reason text when
            // the user selects the row.
        }
    }
}

/// Short stats string like "+12 −3" for a file.
fn file_stats_str(file: &FileDiff) -> String {
    match &file.kind {
        FileKind::Text {
            lines_added,
            lines_removed,
            truncated,
            ..
        } => {
            let mut s = format!("+{lines_added} −{lines_removed}");
            if *truncated {
                s.push_str(" …");
            }
            s
        }
        FileKind::Image { ext, .. } => format!("[{ext}]"),
        FileKind::Binary => "[bin]".into(),
        FileKind::TooLarge => "[large]".into(),
    }
}

fn file_row_label(file: &FileDiff) -> RichText {
    let glyph = file.status.glyph();
    let color = status_color(file.status);
    let mut s = format!("{glyph}  {}", file.display_path());
    match &file.kind {
        FileKind::Text {
            lines_added,
            lines_removed,
            truncated,
            ..
        } => {
            s.push_str(&format!("  (+{lines_added} −{lines_removed}"));
            if *truncated {
                s.push_str(", truncated");
            }
            s.push(')');
        }
        FileKind::Image { ext, .. } => {
            s.push_str(&format!("  [image:{ext}]"));
        }
        FileKind::Binary => {
            s.push_str("  [binary]");
        }
        FileKind::TooLarge => {
            s.push_str("  [too large to show]");
        }
    }
    RichText::new(s).color(color).monospace()
}

fn status_color(s: DeltaStatus) -> Color32 {
    match s {
        DeltaStatus::Added => Color32::from_rgb(90, 200, 120),
        DeltaStatus::Deleted => Color32::from_rgb(220, 110, 110),
        DeltaStatus::Modified => Color32::from_rgb(210, 200, 120),
        DeltaStatus::Renamed | DeltaStatus::Copied => Color32::from_rgb(150, 170, 230),
        DeltaStatus::Typechange => Color32::from_rgb(180, 150, 230),
        DeltaStatus::Unmodified => Color32::GRAY,
    }
}

fn selected_image_cache(ws: &mut WorkspaceState, file: &FileDiff) -> Option<SelectedImageCache> {
    let FileKind::Image { ext } = &file.kind else {
        ws.set_image_cache(None);
        return None;
    };
    let ext = ext.clone();

    let cache_matches = ws
        .selected_image_cache
        .as_ref()
        .map(|cache| {
            cache.old_oid == file.old_oid && cache.new_oid == file.new_oid && cache.ext == ext
        })
        .unwrap_or(false);
    if !cache_matches {
        let old_bytes_raw = crate::git::diff::load_blob_bytes(ws.repo.gix(), file.old_oid);
        let new_bytes_raw = crate::git::diff::load_blob_bytes(ws.repo.gix(), file.new_oid);
        // Extended formats (TGA / PSD / EXR / HDR / QOI) aren't served
        // by egui_extras's built-in loaders. We decode them with the
        // `image` crate (or the PSD embedded-thumbnail parser) and
        // re-encode as PNG so the cached bytes are always in a format
        // the loader can render. PNG stored here is the canonical
        // "preview" representation; the user still sees file size/ext
        // from the original blob further down.
        let (old_bytes, new_bytes, stored_ext) =
            if matches!(ext.as_str(), "tga" | "psd" | "exr" | "hdr" | "qoi") {
                let old_converted = old_bytes_raw
                    .as_ref()
                    .and_then(|b| convert_extended_to_png(b, &ext));
                let new_converted = new_bytes_raw
                    .as_ref()
                    .and_then(|b| convert_extended_to_png(b, &ext));
                (old_converted, new_converted, "png".to_string())
            } else {
                (old_bytes_raw, new_bytes_raw, ext.clone())
            };
        ws.set_image_cache(Some(SelectedImageCache {
            old_oid: file.old_oid,
            new_oid: file.new_oid,
            old_bytes,
            new_bytes,
            ext: stored_ext,
        }));
    }
    ws.selected_image_cache.clone()
}

/// Decode an extended image format to an in-memory PNG. Returns
/// `None` on decode failure so the caller can gracefully fall back to
/// the "image unavailable" placeholder instead of propagating the
/// error up the frame loop.
fn convert_extended_to_png(bytes: &[u8], ext: &str) -> Option<std::sync::Arc<[u8]>> {
    use crate::ui::file_preview::{DecodedImage, FormatKind, PreviewMode};
    let decoded: Option<DecodedImage> = match ext {
        "psd" => crate::ui::file_preview::decode_psd_for_diff_pane(bytes).ok(),
        _ => crate::ui::file_preview::decode_image_for_diff_pane(bytes).ok(),
    };
    let _ = (FormatKind::from_ext(ext), PreviewMode::Full);
    let decoded = decoded?;
    let img = image::RgbaImage::from_raw(decoded.width, decoded.height, decoded.rgba)?;
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut out),
            image::ImageFormat::Png,
        )
        .ok()?;
    Some(out.into())
}

/// One row in the virtualized diff list. Every row has the same fixed
/// height (`DIFF_ROW_HEIGHT`) so `ScrollArea::show_rows` can address it
/// by index without materializing off-screen rows.
enum DiffRow<'a> {
    HunkHeader(&'a str),
    Line(&'a DiffLine),
    Truncated,
}

const DIFF_ROW_HEIGHT: f32 = 16.0;

fn render_file_detail(
    ui: &mut egui::Ui,
    file: &FileDiff,
    image_cache: Option<&SelectedImageCache>,
) {
    match &file.kind {
        FileKind::Text {
            hunks, truncated, ..
        } => {
            if hunks.is_empty() {
                ui.weak("(no textual changes — file might be a pure rename)");
                return;
            }
            // Flatten hunks into a uniform-height row list so we can use
            // egui's row-based virtualization. Previously we laid out every
            // hunk + every line every frame, which on thousand-line diffs
            // meant a full re-layout pass each paint.
            let mut rows: Vec<DiffRow> = Vec::with_capacity(
                hunks.iter().map(|h| h.lines.len() + 1).sum::<usize>() + usize::from(*truncated),
            );
            for hunk in hunks {
                rows.push(DiffRow::HunkHeader(&hunk.header));
                for line in &hunk.lines {
                    rows.push(DiffRow::Line(line));
                }
            }
            if *truncated {
                rows.push(DiffRow::Truncated);
            }
            let total = rows.len();
            ScrollArea::vertical()
                .id_salt("diff_file_detail")
                .auto_shrink([false, false])
                .show_rows(ui, DIFF_ROW_HEIGHT, total, |ui, range| {
                    for i in range {
                        paint_diff_row(ui, &rows[i]);
                    }
                });
        }
        FileKind::Image { ext } => {
            let (old_bytes, new_bytes) = image_cache
                .map(|cache| (cache.old_bytes.clone(), cache.new_bytes.clone()))
                .unwrap_or((None, None));
            ScrollArea::both()
                .id_salt("diff_file_detail_image")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    render_image_diff(ui, file, old_bytes, new_bytes, ext);
                });
        }
        FileKind::Binary => {
            ui.vertical(|ui| {
                ui.weak("Binary file change.");
                ui.monospace(format!(
                    "old: {} bytes   new: {} bytes",
                    file.old_size, file.new_size
                ));
            });
        }
        FileKind::TooLarge => {
            ui.vertical(|ui| {
                ui.weak(format!(
                    "File too large to render inline (>{} MB).",
                    crate::git::diff::MAX_BLOB_BYTES / (1024 * 1024)
                ));
                ui.monospace(format!(
                    "old: {} bytes   new: {} bytes",
                    file.old_size, file.new_size
                ));
            });
        }
    }
}

fn paint_diff_row(ui: &mut egui::Ui, row: &DiffRow<'_>) {
    match row {
        DiffRow::HunkHeader(header) => {
            let (rect, _) = ui.allocate_exact_size(
                Vec2::new(ui.available_width(), DIFF_ROW_HEIGHT),
                egui::Sense::hover(),
            );
            ui.painter().text(
                rect.min + Vec2::new(4.0, 1.0),
                egui::Align2::LEFT_TOP,
                header,
                FontId::monospace(12.5),
                Color32::from_rgb(110, 170, 220),
            );
        }
        DiffRow::Line(line) => paint_diff_line(ui, line),
        DiffRow::Truncated => {
            let (rect, _) = ui.allocate_exact_size(
                Vec2::new(ui.available_width(), DIFF_ROW_HEIGHT),
                egui::Sense::hover(),
            );
            ui.painter().text(
                rect.min + Vec2::new(4.0, 1.0),
                egui::Align2::LEFT_TOP,
                "… diff truncated (hit internal line cap).",
                FontId::monospace(12.5),
                Color32::from_rgb(220, 180, 120),
            );
        }
    }
}

fn render_file_snapshot(
    ui: &mut egui::Ui,
    ws: &mut WorkspaceState,
    file: &FileDiff,
    image_cache: Option<&SelectedImageCache>,
) {
    match &file.kind {
        FileKind::Text { .. } => {
            let oid = file.new_oid.or(file.old_oid);
            // Populate / refresh the snapshot cache if the selected blob
            // changed. Loading + line-indexing is O(blob_bytes) — done once
            // per selection, not per frame.
            let needs_rebuild = ws
                .snapshot_cache
                .as_ref()
                .map(|cache| cache.oid != oid)
                .unwrap_or(true);
            if needs_rebuild {
                let text = crate::git::diff::load_blob_text(ws.repo.gix(), oid);
                match text {
                    Some(text) if !text.is_empty() => {
                        let line_bounds = compute_line_bounds(&text);
                        ws.snapshot_cache = Some(SnapshotCache {
                            oid,
                            text: Arc::<str>::from(text),
                            line_bounds,
                        });
                    }
                    Some(_) => {
                        // Empty blob — cache an empty sentinel so we don't
                        // re-hit the object DB every frame.
                        ws.snapshot_cache = Some(SnapshotCache {
                            oid,
                            text: Arc::<str>::from(""),
                            line_bounds: Vec::new(),
                        });
                    }
                    None => {
                        ws.snapshot_cache = None;
                        ui.weak("Could not load file contents for this commit.");
                        return;
                    }
                }
            }
            let Some(cache) = ws.snapshot_cache.as_ref() else {
                ui.weak("Could not load file contents for this commit.");
                return;
            };
            if cache.line_bounds.is_empty() {
                ui.weak("(empty file)");
                return;
            }
            render_highlighted_snapshot(ui, file, cache);
        }
        FileKind::Image { ext } => {
            // The snapshot ("File View") is the natural home for a
            // single-image preview: we're looking at *this file as of
            // this commit*, not a before/after comparison. We route
            // through the async preview pipeline (`PreviewManager`) so
            // large textures don't block the UI thread on decode the
            // way the raw `image_panel` loader does.
            let bytes = image_cache
                .and_then(|cache| cache.new_bytes.clone().or(cache.old_bytes.clone()));
            let oid = file.new_oid.or(file.old_oid);
            let caption = if file.new_oid.is_some() {
                "File at selected commit"
            } else {
                "Deleted file (previous contents)"
            };
            render_single_image_preview(
                ui,
                "snapshot_image",
                caption,
                bytes,
                oid,
                ext,
                file.new_size.max(file.old_size),
            );
        }
        FileKind::Binary => {
            ui.weak("Binary file snapshot is not shown inline.");
            ui.monospace(format!(
                "old: {} bytes   new: {} bytes",
                file.old_size, file.new_size
            ));
        }
        FileKind::TooLarge => {
            ui.weak(format!(
                "File snapshot is too large to render inline (>{} MB).",
                crate::git::diff::MAX_BLOB_BYTES / (1024 * 1024)
            ));
            ui.monospace(format!(
                "old: {} bytes   new: {} bytes",
                file.old_size, file.new_size
            ));
        }
    }
}

/// Build the (start, end) byte offset for each line in `text`. We store
/// these in `SnapshotCache.line_bounds` so the virtualized renderer can
/// address line `i` in O(1) without re-walking the file every frame.
pub(crate) fn compute_line_bounds(text: &str) -> Vec<(u32, u32)> {
    let mut bounds = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            let end = if i > start && bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            bounds.push((start as u32, end as u32));
            start = i + 1;
        }
    }
    if start < bytes.len() {
        bounds.push((start as u32, bytes.len() as u32));
    }
    bounds
}

const SNAPSHOT_ROW_HEIGHT: f32 = 18.0;
const SNAPSHOT_GUTTER_WIDTH: f32 = 52.0;

fn render_highlighted_snapshot(ui: &mut egui::Ui, file: &FileDiff, cache: &SnapshotCache) {
    let path = file.display_path();
    render_text_snapshot(ui, &path, &cache.text, &cache.line_bounds);
}

fn render_text_snapshot(ui: &mut egui::Ui, path: &str, text: &str, bounds: &[(u32, u32)]) {
    let gutter_color = ui.visuals().widgets.noninteractive.fg_stroke.color;
    let line_bg = ui.visuals().faint_bg_color.gamma_multiply(0.22);
    let total = bounds.len();

    ScrollArea::vertical()
        .id_salt("snapshot_text")
        .auto_shrink([false, false])
        .show_rows(ui, SNAPSHOT_ROW_HEIGHT, total, |ui, range| {
            for i in range {
                let (start, end) = bounds[i];
                let line = &text[start as usize..end as usize];
                ui.horizontal(|ui| {
                    let (gutter_rect, _) = ui.allocate_exact_size(
                        Vec2::new(SNAPSHOT_GUTTER_WIDTH, SNAPSHOT_ROW_HEIGHT),
                        egui::Sense::hover(),
                    );
                    ui.painter().rect_filled(gutter_rect, 4.0, line_bg);
                    ui.painter().text(
                        gutter_rect.right_center() - Vec2::new(8.0, 0.0),
                        egui::Align2::RIGHT_CENTER,
                        format!("{:>4}", i + 1),
                        FontId::monospace(12.0),
                        gutter_color.gamma_multiply(0.78),
                    );

                    let job =
                        crate::ui::syntax::highlighted_code_job(line, Some(path), ui.visuals());
                    ui.add(egui::Label::new(job).sense(egui::Sense::hover()));
                });
            }
        });
}

fn paint_diff_line(ui: &mut egui::Ui, line: &DiffLine) {
    // Each line is rendered as a full-width row with a background colour
    // and a fixed-width gutter showing old/new line numbers, then the
    // content in monospace. We don't try to do word-level intra-line
    // diffing for MVP — column-oriented +/− is enough to read.
    let (bg, fg, prefix) = match line.kind {
        LineKind::Add => (
            Color32::from_rgba_unmultiplied(80, 180, 100, 38),
            Color32::from_rgb(170, 230, 180),
            '+',
        ),
        LineKind::Remove => (
            Color32::from_rgba_unmultiplied(220, 110, 110, 42),
            Color32::from_rgb(240, 180, 180),
            '-',
        ),
        LineKind::Meta => (Color32::TRANSPARENT, Color32::DARK_GRAY, '·'),
        LineKind::Context => (Color32::TRANSPARENT, Color32::LIGHT_GRAY, ' '),
    };

    let old = line
        .old_lineno
        .map(|n| format!("{n:>4}"))
        .unwrap_or_else(|| "    ".into());
    let new = line
        .new_lineno
        .map(|n| format!("{n:>4}"))
        .unwrap_or_else(|| "    ".into());

    let font = FontId::monospace(12.5);
    let text = format!(" {old} {new} {prefix} {}", line.content);

    // Allocate a full-width row, paint background, then draw text.
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 16.0), egui::Sense::hover());
    if bg.a() > 0 {
        ui.painter().rect_filled(rect, 0.0, bg);
    }
    ui.painter().text(
        rect.min + Vec2::new(4.0, 2.0),
        egui::Align2::LEFT_TOP,
        text,
        font,
        fg,
    );
}

/// Side-by-side image diff. Handles add (new only), delete (old only), and
/// modify (both). Each image gets a border + caption with its byte size;
/// We lazily install egui's image loader only when the selected file is an
/// actual image diff, which keeps startup and idle memory lower.
fn render_image_diff(
    ui: &mut egui::Ui,
    file: &FileDiff,
    old: Option<Arc<[u8]>>,
    new: Option<Arc<[u8]>>,
    ext: &str,
) {
    ui.horizontal_wrapped(|ui| {
        if let Some(bytes) = &old {
            image_panel(
                ui,
                "Before (old)",
                bytes.clone(),
                file.old_size,
                ext,
                file.old_oid,
            );
        } else {
            empty_panel(ui, "Before — N/A");
        }
        if let Some(bytes) = &new {
            image_panel(
                ui,
                "After (new)",
                bytes.clone(),
                file.new_size,
                ext,
                file.new_oid,
            );
        } else {
            empty_panel(ui, "After — N/A");
        }
    });
}

fn image_panel(
    ui: &mut egui::Ui,
    caption: &str,
    bytes: Arc<[u8]>,
    size: usize,
    ext: &str,
    oid: Option<gix::ObjectId>,
) {
    ui.vertical(|ui| {
        ui.set_width(240.0);
        ui.label(RichText::new(caption).strong());
        let rect = ui.available_rect_before_wrap();
        let _ = rect;

        // `egui::Image::from_bytes` needs a unique URI per distinct payload.
        // We use the blob oid if we have one, falling back to a pointer hash
        // so the loader cache still works for distinct blobs.
        let uri = match oid {
            Some(o) => format!("bytes://diff/{o}.{ext}"),
            None => format!(
                "bytes://diff/anon-{:x}.{ext}",
                bytes.as_ptr() as usize as u64
            ),
        };

        let img = egui::Image::from_bytes(uri, egui::load::Bytes::Shared(bytes))
            .fit_to_exact_size(Vec2::new(220.0, 220.0))
            .maintain_aspect_ratio(true)
            .max_size(Vec2::new(220.0, 220.0));

        // Border for contrast.
        egui::Frame::none()
            .stroke(Stroke::new(1.0, Color32::from_gray(80)))
            .inner_margin(4.0)
            .show(ui, |ui| {
                ui.add(img);
            });
        ui.small(format!("{size} bytes · .{ext}"));
    });
}

/// Zoom/pan state for the single-image preview pane. Kept in egui's
/// per-context memory keyed by the pane id so state survives frame-to-
/// frame redraws but auto-resets when the user switches to a different
/// file (different id = different entry).
///
/// `zoom = 0` is a sentinel for "not initialised yet; fit to pane on
/// the next frame" — we can't compute fit during first paint because
/// the pane rect isn't known until allocation.
#[derive(Debug, Clone, Copy)]
struct ImageZoomPan {
    /// Absolute zoom: 1.0 = 1 image-pixel per screen-pixel. Lower =
    /// image is smaller than native, higher = zoomed in.
    zoom: f32,
    /// Pan offset in screen pixels relative to the pane centre.
    pan: egui::Vec2,
    /// True if the user has manually zoomed — future frames should
    /// NOT auto-fit on pane resize (they'd feel possessive otherwise).
    user_zoomed: bool,
}

impl Default for ImageZoomPan {
    fn default() -> Self {
        Self {
            zoom: 0.0,
            pan: egui::Vec2::ZERO,
            user_zoomed: false,
        }
    }
}

/// Single-image preview with mouse-wheel zoom + drag-to-pan + metadata.
/// Feeds through `PreviewManager` so decode is async and large textures
/// don't block the UI.
///
/// Design notes:
/// * The pane is always rendered (even when the image is pending /
///   failed) so the surrounding layout is stable across the decode
///   lifecycle — no snap when the image lands.
/// * Zoom uses the mouse position as the anchor: scrolling the wheel
///   over a specific pixel keeps that pixel under the cursor. This is
///   what every modern image viewer does and it's worth the small
///   cost over naive centre-zoom.
/// * Metadata row lives *below* the image with the caption on top so
///   the eye falls through the image first, then reads context. Flips
///   from the old side-by-side layout which led with the caption.
fn render_single_image_preview(
    ui: &mut egui::Ui,
    id_salt: &str,
    caption: &str,
    bytes: Option<Arc<[u8]>>,
    oid: Option<gix::ObjectId>,
    ext: &str,
    size_bytes: usize,
) {
    use crate::ui::file_preview::{
        PreviewIdentity, PreviewKey, PreviewManager, PreviewMode, PreviewState,
    };

    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(caption).strong());
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Fit").on_hover_text("Fit to pane").clicked() {
                    // Fit button resets both zoom and pan; clearing
                    // user_zoomed re-enables auto-fit on resize.
                    let id = ui.make_persistent_id((id_salt, "zoompan"));
                    ui.ctx().memory_mut(|m| {
                        m.data.insert_temp(id, ImageZoomPan::default());
                    });
                }
            });
        });

        let Some(bytes) = bytes else {
            ui.weak("Could not load image contents.");
            return;
        };

        // Identity for the preview pipeline. Anonymous (no oid) paths
        // hash the pointer so working-tree edits still dedupe within
        // one edit session.
        let identity = match oid {
            Some(o) => PreviewIdentity::Blob(o),
            None => PreviewIdentity::Blob(synthetic_oid_for_ptr(bytes.as_ptr() as usize)),
        };
        let key = PreviewKey {
            identity,
            mode: PreviewMode::Full,
        };
        let manager = PreviewManager::global();
        let state = match manager.peek(&key) {
            Some(s) => s,
            None => {
                let pseudo_oid = match oid {
                    Some(o) => o,
                    None => synthetic_oid_for_ptr(bytes.as_ptr() as usize),
                };
                manager.request_blob(pseudo_oid, bytes.clone(), ext, PreviewMode::Full)
            }
        };

        // Reserve the remaining vertical space minus a small footer
        // area for metadata. A fixed max keeps enormous 8k textures
        // from eating the entire pane.
        let avail = ui.available_size();
        let pane_size = egui::vec2(avail.x.max(200.0), (avail.y - 36.0).max(200.0));
        let (rect, resp) = ui.allocate_exact_size(pane_size, egui::Sense::click_and_drag());
        ui.painter()
            .rect_stroke(rect, 2.0, Stroke::new(1.0, Color32::from_gray(70)));

        match state {
            PreviewState::Ready(ref img) => {
                let tex_name = format!(
                    "preview-full-{}",
                    oid.map(|o| o.to_string())
                        .unwrap_or_else(|| "anon".to_string())
                );
                if let Some(tex) = manager.texture_for(ui.ctx(), &key, &tex_name) {
                    draw_image_with_zoom_pan(ui, rect, &resp, &tex, id_salt);
                }
                ui.small(format!(
                    "{w} × {h} · {size} bytes · .{ext}",
                    w = img.width,
                    h = img.height,
                    size = size_bytes,
                ));
            }
            PreviewState::Pending => {
                ui.painter()
                    .rect_filled(rect, 2.0, ui.visuals().faint_bg_color);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "Decoding…",
                    egui::FontId::proportional(13.0),
                    ui.visuals().weak_text_color(),
                );
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(50));
                ui.small(format!("{size_bytes} bytes · .{ext}"));
            }
            PreviewState::Failed { reason } => {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("Preview failed: {reason}"),
                    egui::FontId::proportional(13.0),
                    Color32::from_rgb(220, 140, 120),
                );
                ui.small(format!("{size_bytes} bytes · .{ext}"));
            }
            PreviewState::TooLarge { bytes: n } => {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!(
                        "Image too large to preview ({n} bytes > {} MB cap)",
                        crate::ui::file_preview::MAX_INPUT_BYTES / (1024 * 1024)
                    ),
                    egui::FontId::proportional(13.0),
                    Color32::from_rgb(220, 180, 90),
                );
                ui.small(format!("{size_bytes} bytes · .{ext}"));
            }
            PreviewState::Unsupported { label } => {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    label,
                    egui::FontId::proportional(13.0),
                    ui.visuals().weak_text_color(),
                );
                ui.small(format!("{size_bytes} bytes · .{ext}"));
            }
        }
    });
}

/// Build a deterministic synthetic `ObjectId` from a pointer. We only
/// use this to feed `request_blob` — it doesn't have to be a real git
/// object, just stable across frames for the same `Arc<[u8]>`.
fn synthetic_oid_for_ptr(ptr: usize) -> gix::ObjectId {
    let mut bytes = [0u8; 20];
    let p = (ptr as u64).to_le_bytes();
    bytes[..8].copy_from_slice(&p);
    // Tag remaining bytes so a collision with a real SHA-1 is vanishingly
    // unlikely. "MERGEFOXSYNT" fills 12 bytes.
    bytes[8..20].copy_from_slice(b"MERGEFOXSYNT");
    gix::ObjectId::from_bytes_or_panic(&bytes)
}

/// Actually draw the texture, applying the active zoom/pan and
/// updating it from mouse events this frame. Called only when the
/// preview is in the `Ready` state and we have a `TextureHandle`.
fn draw_image_with_zoom_pan(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    resp: &egui::Response,
    tex: &egui::TextureHandle,
    id_salt: &str,
) {
    let id = ui.make_persistent_id((id_salt, "zoompan"));
    let mut zp: ImageZoomPan = ui
        .ctx()
        .memory(|m| m.data.get_temp::<ImageZoomPan>(id))
        .unwrap_or_default();
    let tex_size = tex.size_vec2();

    // Auto-fit on first frame (or after user pressed Fit).
    if zp.zoom <= 0.0 {
        let fit = (rect.width() / tex_size.x)
            .min(rect.height() / tex_size.y)
            .min(1.0)
            .max(0.01);
        zp.zoom = fit;
        zp.pan = egui::Vec2::ZERO;
        zp.user_zoomed = false;
    }

    // Scroll-wheel zoom with mouse-anchored scaling. We use the raw
    // scroll delta (not smoothed) for direct feel; one tick ≈ 10%.
    if resp.hovered() {
        let scroll = ui.input(|i| i.raw_scroll_delta.y);
        if scroll != 0.0 {
            let factor = (scroll / 120.0).exp2(); // one full "tick" ≈ 2x
            let new_zoom = (zp.zoom * factor).clamp(0.02, 32.0);
            // Anchor around the cursor: offset the pan so the world
            // point under the cursor stays under the cursor.
            if let Some(cursor) = resp.hover_pos() {
                let centre = rect.center() + zp.pan;
                let world_under_cursor = (cursor - centre) / zp.zoom;
                let new_centre = cursor - world_under_cursor * new_zoom;
                zp.pan = new_centre - rect.center();
            }
            zp.zoom = new_zoom;
            zp.user_zoomed = true;
        }
    }

    // Drag-to-pan.
    if resp.dragged() {
        zp.pan += resp.drag_delta();
    }

    // Compute draw rect and blit.
    let draw_size = tex_size * zp.zoom;
    let centre = rect.center() + zp.pan;
    let draw_rect = egui::Rect::from_center_size(centre, draw_size);
    // Clip to pane so zoomed-in images don't bleed over neighbouring
    // widgets (e.g. the metadata footer below).
    let painter = ui.painter_at(rect);
    painter.image(
        tex.id(),
        draw_rect,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );

    // Persist updated state for the next frame.
    ui.ctx().memory_mut(|m| m.data.insert_temp(id, zp));
}

fn empty_panel(ui: &mut egui::Ui, caption: &str) {
    ui.vertical(|ui| {
        ui.set_width(240.0);
        ui.label(RichText::new(caption).strong());
        egui::Frame::none()
            .stroke(Stroke::new(1.0, Color32::from_gray(60)))
            .inner_margin(4.0)
            .show(ui, |ui| {
                ui.allocate_exact_size(Vec2::new(220.0, 220.0), egui::Sense::hover());
            });
    });
}
