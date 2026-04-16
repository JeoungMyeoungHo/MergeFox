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
const FILE_ROW_HEIGHT: f32 = 20.0;

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
    if !show_working_tree_panel && diff.is_none() && ws.diff_task.is_none() {
        // Nothing to show and nothing computing — don't render the panel.
        return;
    }

    let mut close = false;

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
                render_commit_summary(ui, &diff);
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
            ui.heading("Working Tree");
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                let close_button =
                    egui::Button::new(RichText::new("x").strong().size(16.0).monospace())
                        .frame(true);
                if ui
                    .add_sized([30.0, 30.0], close_button)
                    .on_hover_text("Close working tree panel")
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
        ui.weak("Working tree is clean.");
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
                                ws.working_file_diff = crate::git::diff_text_for_working_entry(
                                    ws.repo.path(),
                                    entry,
                                )
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
        if ui.button("← Working Tree").clicked() {
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
    ui.small(format!(
        "Working Tree · {}",
        working_tree_stats_str(&entry)
    ));
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
            ScrollArea::both()
                .id_salt("working_snapshot_image")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let image = image_cache
                        .and_then(|cache| cache.new_bytes.clone().or(cache.old_bytes.clone()));
                    if let Some(bytes) = image {
                        let caption = if file.new_path.is_some() {
                            "Current working tree file"
                        } else {
                            "Deleted file (last committed contents)"
                        };
                        image_panel(
                            ui,
                            caption,
                            bytes,
                            file.new_size.max(file.old_size),
                            ext,
                            None,
                        );
                    } else {
                        ui.weak("Could not load image contents for this working tree file.");
                    }
                });
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

fn render_commit_summary(ui: &mut egui::Ui, diff: &RepoDiff) {
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

    if let Some(author) = diff
        .commit_author
        .as_deref()
        .map(str::trim)
        .filter(|author| !author.is_empty())
    {
        ui.weak(format!("Author: {author}"));
    }
}

/// Flat file list — virtualized, left-aligned glyph + path, right-aligned stats.
/// Full-width click target so selection highlight spans the whole row.
fn render_file_flat(ui: &mut egui::Ui, ws: &mut WorkspaceState, diff: &RepoDiff) {
    let total_files = diff.files.len();
    ScrollArea::vertical()
        .id_salt("diff_files_flat")
        .auto_shrink([false, false])
        .show_rows(ui, FILE_ROW_HEIGHT, total_files, |ui, range| {
            let row_width = ui.available_width();
            for i in range {
                let file = &diff.files[i];
                let selected = ws.selected_file_idx == Some(i);
                let color = status_color(file.status);
                // Allocate a full-width row for consistent click target + highlight.
                let (rect, resp) = ui.allocate_exact_size(
                    egui::vec2(row_width, FILE_ROW_HEIGHT),
                    egui::Sense::click(),
                );
                // Selection / hover background.
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
                // Left: glyph + path.
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
                child.add(
                    egui::Label::new(RichText::new(file.display_path()).monospace())
                        .truncate()
                        .selectable(false),
                );
                // Right: stats.
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
fn render_file_tree(ui: &mut egui::Ui, ws: &mut WorkspaceState, diff: &RepoDiff) {
    // Build a simple directory → file-indices map. We preserve the
    // original index so clicking a tree leaf sets the correct
    // `selected_file_idx` into `diff.files`.
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
                            RichText::new(file.status.glyph())
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
        ws.set_image_cache(Some(SelectedImageCache {
            old_oid: file.old_oid,
            new_oid: file.new_oid,
            old_bytes: crate::git::diff::load_blob_bytes(ws.repo.gix(), file.old_oid),
            new_bytes: crate::git::diff::load_blob_bytes(ws.repo.gix(), file.new_oid),
            ext,
        }));
    }
    ws.selected_image_cache.clone()
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
            ScrollArea::both()
                .id_salt("snapshot_image")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let image = image_cache
                        .and_then(|cache| cache.new_bytes.clone().or(cache.old_bytes.clone()));
                    if let Some(bytes) = image {
                        let caption = if file.new_oid.is_some() {
                            "File at selected commit"
                        } else {
                            "Deleted file (previous contents)"
                        };
                        image_panel(
                            ui,
                            caption,
                            bytes,
                            file.new_size.max(file.old_size),
                            ext,
                            file.new_oid.or(file.old_oid),
                        );
                    } else {
                        ui.weak("Could not load image contents for this commit.");
                    }
                });
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

fn render_text_snapshot(
    ui: &mut egui::Ui,
    path: &str,
    text: &str,
    bounds: &[(u32, u32)],
) {
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

                    let job = crate::ui::syntax::highlighted_code_job(
                        line,
                        Some(path),
                        ui.visuals(),
                    );
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
