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
use crate::git::hunk_staging::{self, DiffSide};
use crate::git::{
    DeltaStatus, DiffLine, EntryKind, FileDiff, FileKind, LineKind, RepoDiff, StatusEntry,
};

/// A hunk-level user intent collected while rendering the diff panel.
///
/// The diff view can't perform the actual git operation inline — it
/// borrows `WorkspaceState` mutably for rendering, and the operations
/// need to refresh the working-tree cache afterwards (which borrows
/// `MergeFoxApp`). Instead, we record what the user asked for here and
/// let the outer `MergeFoxApp::update` loop apply it after the paint
/// closure releases its borrow.
#[derive(Debug, Clone)]
pub enum HunkAction {
    /// Stage a single hunk (or line subset) from the unstaged side.
    StageHunk {
        file: std::path::PathBuf,
        hunk_index: usize,
        line_indices: Vec<usize>,
    },
    /// Unstage a single hunk from the staged side.
    UnstageHunk {
        file: std::path::PathBuf,
        hunk_index: usize,
        line_indices: Vec<usize>,
    },
    /// Discard a hunk from the working tree. Surfaced through a
    /// confirmation prompt before the op actually runs.
    DiscardHunk {
        file: std::path::PathBuf,
        hunk_index: usize,
        line_indices: Vec<usize>,
    },
    /// "Stage selected lines" toolbar: apply each affected hunk with
    /// the current `HunkSelectionState` as-is, then clear the
    /// selection.
    StageSelectedLines { file: std::path::PathBuf },
    /// Symmetric — unstage only the currently-selected lines.
    UnstageSelectedLines { file: std::path::PathBuf },
}

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
                        // Leave the side-specific diff text empty so
                        // `render_working_file_center` fetches the
                        // right side (unstaged vs staged) on next
                        // paint. Computing a combined HEAD-diff here
                        // would be wasted work and would desync from
                        // the hunk-staging flow.
                        ws.working_file_diff = None;
                    }
                    ws.selected_file_view = SelectedFileView::Diff;
                    ws.set_image_cache(None);
                }

                resp.on_hover_text(entry.path.display().to_string());
            }
        });
}

/// Aggregate a folder's status entries into a tri-state staging flag.
/// `Mixed` is the result when entries disagree, OR when any single entry has
/// *both* staged and unstaged portions (a partially-staged file) — cycling
/// that through "stage all" or "unstage all" resolves it, which is the
/// idiom most file-browser-style staging UIs follow.
fn folder_stage_state(entries: &[&StatusEntry]) -> FolderStage {
    // Untracked files can't be "unstaged" — they're outside the index. We
    // treat them as "not staged" for the purposes of the folder checkbox so
    // a folder of all-new files starts at FolderStage::None.
    let mut any_staged = false;
    let mut any_unstaged = false;
    for e in entries {
        // Conflicted files don't participate in the staging toggle — we leave
        // them to the conflict-resolution flow. Treat as mixed-ish by forcing
        // Mixed if any conflicted entry exists.
        if e.conflicted {
            return FolderStage::Mixed;
        }
        if e.staged {
            any_staged = true;
        }
        if e.unstaged || matches!(e.kind, EntryKind::Untracked) {
            any_unstaged = true;
        }
    }
    match (any_staged, any_unstaged) {
        (true, false) => FolderStage::All,
        (false, true) => FolderStage::None,
        (false, false) => FolderStage::None, // empty folder — shouldn't happen
        (true, true) => FolderStage::Mixed,
    }
}

/// Working-tree tree view with folder headers and per-folder tri-state
/// staging checkboxes. Clicking a folder checkbox cycles:
///   `Mixed`  → clicking stages every file in the folder (`FolderStage::All`)
///   `All`    → clicking unstages every file in the folder (`FolderStage::None`)
///   `None`   → clicking stages every file in the folder (`FolderStage::All`)
/// This matches git-gui's default — resolving a mixed state by staging
/// everything aligns with the common "I want this whole folder in my
/// next commit" workflow.
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

    // Collect folder-level staging requests here and apply them *after* the
    // render loop so we don't mutate the working-tree cache while still
    // reading it. Each request is a (paths, should_stage) tuple.
    let mut stage_requests: Vec<(Vec<std::path::PathBuf>, bool)> = Vec::new();

    ScrollArea::vertical()
        .id_salt("working_tree_files_tree")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (dir, files) in &dirs {
                let dir_label = if dir == "." {
                    "(root)".to_string()
                } else {
                    dir.clone()
                };

                let mut stats = FolderStats::default();
                for e in files {
                    // Map `EntryKind` → `DeltaStatus` for stats reuse. The
                    // mapping is lossy (Typechange, Untracked collapse to
                    // "other"/"added") but drives display only.
                    let ds = match e.kind {
                        EntryKind::New | EntryKind::Untracked => DeltaStatus::Added,
                        EntryKind::Modified => DeltaStatus::Modified,
                        EntryKind::Deleted => DeltaStatus::Deleted,
                        EntryKind::Renamed => DeltaStatus::Renamed,
                        EntryKind::Typechange => DeltaStatus::Typechange,
                        EntryKind::Conflicted => DeltaStatus::Unmodified,
                    };
                    stats.push(ds);
                }

                let stage = folder_stage_state(files);

                let open_id = ui.id().with(("wt_tree_open", dir));
                let mut open = ui
                    .ctx()
                    .data(|d| d.get_temp::<bool>(open_id))
                    .unwrap_or(true);

                let (toggled, cb_resp) =
                    folder_header_row(ui, open, &dir_label, &stats, Some(stage));
                if toggled {
                    open = !open;
                    ui.ctx().data_mut(|d| d.insert_temp(open_id, open));
                }
                if let Some(resp) = cb_resp {
                    if resp.clicked() {
                        // Cycle: Mixed → All, None → All, All → None.
                        let should_stage = !matches!(stage, FolderStage::All);
                        let paths: Vec<std::path::PathBuf> =
                            files.iter().map(|e| e.path.clone()).collect();
                        stage_requests.push((paths, should_stage));
                    }
                }
                if !open {
                    continue;
                }
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
                            .max_rect(rect.shrink2(egui::vec2(16.0, 0.0)))
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
            }
        });

    // Apply deferred folder-level staging operations. We call the git-ops
    // layer directly (which is allowed — only app.rs and a few other files
    // are off-limits) and then force the next working-tree poll to rerun so
    // the UI reflects the change without a full frame of stale state.
    for (paths, stage_it) in stage_requests {
        let path_refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_path()).collect();
        let result = if stage_it {
            crate::git::ops::stage_paths(ws.repo.path(), &path_refs)
        } else {
            crate::git::ops::unstage_paths(ws.repo.path(), &path_refs)
        };
        if let Err(err) = result {
            // Surface the error into the working-tree error slot so the user
            // sees it (the panel already renders `working_error`). We don't
            // reach into app.rs to show a toast — that would require touching
            // forbidden files.
            if let Some(cache) = ws.repo_ui_cache.as_mut() {
                cache.working_error = Some(format!(
                    "Folder stage/unstage failed: {}",
                    err
                ));
            }
        } else {
            // Force the next poll to refresh immediately by rewinding the
            // last-poll timestamp past the poll interval. We don't have
            // access to `WORKING_TREE_POLL_INTERVAL` (private to app.rs), but
            // subtracting a generous 5 s guarantees we're past any sane value.
            ws.last_working_tree_poll = std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(5))
                .unwrap_or_else(std::time::Instant::now);
        }
    }
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

pub(crate) fn show_selected_file_center(
    ui: &mut egui::Ui,
    ws: &mut WorkspaceState,
    diff_prefs: &mut crate::config::DiffPrefs,
) {
    // Working Tree file selected: render it inline
    if ws.selected_working_tree {
        if let Some(path) = ws.selected_working_file.clone() {
            render_working_file_center(ui, ws, &path, diff_prefs);
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
            // Minimap toggle — only meaningful in Diff view, but
            // kept visible in both so its presence doesn't flicker
            // across toggles. A no-op click in File view is a
            // cheaper trade than a jumping toolbar.
            minimap_toggle_button(ui, diff_prefs);
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
        SelectedFileView::Diff => render_file_detail(ui, &file, image_cache.as_ref(), diff_prefs),
        SelectedFileView::File => {
            render_file_snapshot(ui, ws, &file, image_cache.as_ref());
        }
    }
}

/// Render a working tree file (staged or unstaged) in the center pane.
fn render_working_file_center(
    ui: &mut egui::Ui,
    ws: &mut WorkspaceState,
    path: &std::path::Path,
    diff_prefs: &mut crate::config::DiffPrefs,
) {
    let Some(entry) = selected_working_entry(ws, path) else {
        ws.selected_working_file = None;
        ws.working_file_diff = None;
        ws.hunk_selection.reset_to(None, DiffSide::Unstaged);
        ui.vertical_centered(|ui| {
            ui.weak("Select a file from the right panel to open its diff or file view.")
        });
        return;
    };
    let path_str = path.display().to_string();

    // Keep the line-selection buffer anchored on the currently-shown
    // (file, side). Switching files or toggling staged ↔ unstaged
    // should NOT carry stale picks forward, otherwise "Stage 3 lines"
    // could silently reach into a file the user no longer sees.
    let needs_reset = ws
        .hunk_selection
        .file
        .as_deref()
        .map(|f| f != path)
        .unwrap_or(true)
        || ws.hunk_selection.side != ws.working_diff_side;
    if needs_reset {
        ws.hunk_selection
            .reset_to(Some(path.to_path_buf()), ws.working_diff_side);
    }

    // Which sides actually have content? An unstaged-only file has
    // nothing on the staged side, so the toggle for Staged is hidden
    // to avoid giving the user a dead tab. Pure-staged files have the
    // inverse layout.
    let has_unstaged = entry.unstaged || matches!(entry.kind, EntryKind::Untracked);
    let has_staged = entry.staged;
    // If the current side has no content, prefer the other.
    if ws.working_diff_side == DiffSide::Staged && !has_staged && has_unstaged {
        ws.working_diff_side = DiffSide::Unstaged;
        ws.hunk_selection
            .reset_to(Some(path.to_path_buf()), DiffSide::Unstaged);
        ws.working_file_diff = None; // force refetch
    } else if ws.working_diff_side == DiffSide::Unstaged && !has_unstaged && has_staged {
        ws.working_diff_side = DiffSide::Staged;
        ws.hunk_selection
            .reset_to(Some(path.to_path_buf()), DiffSide::Staged);
        ws.working_file_diff = None;
    }

    ui.horizontal(|ui| {
        if ui.button("← Changes").clicked() {
            ws.selected_working_file = None;
            ws.set_image_cache(None);
            ws.working_file_diff = None;
            ws.hunk_selection.reset_to(None, DiffSide::Unstaged);
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
            minimap_toggle_button(ui, diff_prefs);
        });
    });
    ui.small(format!("Changes · {}", working_tree_stats_str(&entry)));

    // Staged / Unstaged toggle — only render when both sides exist.
    // Partial-stage files (edits both staged AND unstaged) benefit
    // most from this: the user can review what's already staged, then
    // flip to "Unstaged" to pick the next hunk.
    if has_unstaged && has_staged {
        ui.horizontal(|ui| {
            ui.label("View:");
            for (label, side) in [
                ("Unstaged", DiffSide::Unstaged),
                ("Staged", DiffSide::Staged),
            ] {
                if ui
                    .selectable_label(ws.working_diff_side == side, label)
                    .clicked()
                    && ws.working_diff_side != side
                {
                    ws.working_diff_side = side;
                    ws.hunk_selection
                        .reset_to(Some(path.to_path_buf()), side);
                    ws.working_file_diff = None; // force refetch on next branch
                }
            }
        });
    }

    ui.separator();

    if ws.working_file_diff.is_none() {
        ws.working_file_diff = match ws.working_diff_side {
            DiffSide::Unstaged => {
                crate::git::diff_text_unstaged_only(ws.repo.path(), &entry).ok()
            }
            DiffSide::Staged => {
                crate::git::diff_text_staged_only(ws.repo.path(), &entry).ok()
            }
        };
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
                render_working_diff_with_staging(ui, ws, &entry, file, image_cache.as_ref(), diff_prefs);
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

/// Render the unified diff for a working-tree file with per-hunk and
/// per-line staging controls.
///
/// Unlike `render_file_detail`, which uses egui's row-virtualized
/// renderer for maximum throughput on large commit diffs, this path
/// uses regular widgets for each hunk: buttons, checkboxes, and
/// interactive labels that live inside egui's hit-testing tree. That
/// trades a little raw scroll speed for genuine interactivity — and
/// working-tree diffs are almost always orders of magnitude smaller
/// than commit diffs, so the tradeoff is worth it in practice.
fn render_working_diff_with_staging(
    ui: &mut egui::Ui,
    ws: &mut WorkspaceState,
    entry: &StatusEntry,
    file: &FileDiff,
    image_cache: Option<&SelectedImageCache>,
    diff_prefs: &mut crate::config::DiffPrefs,
) {
    // Fall back to the standard renderer for non-text / disabled
    // cases — binary files and images have no hunk concept.
    if let Some(reason) = hunk_staging::hunk_staging_block_reason(file) {
        ui.horizontal(|ui| {
            ui.colored_label(
                Color32::from_rgb(210, 170, 90),
                format!("Hunk staging disabled: {reason}"),
            );
        });
        render_file_detail(ui, file, image_cache, diff_prefs);
        return;
    }
    if entry.conflicted {
        ui.colored_label(
            Color32::from_rgb(240, 90, 90),
            "Resolve conflicts first — hunk staging is disabled for conflicted files.",
        );
        render_file_detail(ui, file, image_cache, diff_prefs);
        return;
    }

    let hunks = match &file.kind {
        FileKind::Text { hunks, .. } => hunks,
        _ => {
            render_file_detail(ui, file, image_cache, diff_prefs);
            return;
        }
    };
    if hunks.is_empty() {
        ui.weak("(no textual changes — file might be a pure rename)");
        return;
    }

    let side = ws.working_diff_side;
    let path = file
        .new_path
        .clone()
        .or_else(|| file.old_path.clone())
        .unwrap_or_else(|| entry.path.clone());

    // Selection-scoped toolbar — only shown when the user has ticked
    // at least one line checkbox. Keeps the regular case uncluttered.
    let total_selected = ws.hunk_selection.total_selected();
    if total_selected > 0 {
        ui.horizontal(|ui| {
            ui.weak(format!("{total_selected} line(s) selected"));
            ui.separator();
            let action_label = match side {
                DiffSide::Unstaged => format!("Stage {total_selected} lines"),
                DiffSide::Staged => format!("Unstage {total_selected} lines"),
            };
            if ui.button(action_label).clicked() {
                let act = match side {
                    DiffSide::Unstaged => HunkAction::StageSelectedLines {
                        file: path.clone(),
                    },
                    DiffSide::Staged => HunkAction::UnstageSelectedLines {
                        file: path.clone(),
                    },
                };
                ws.pending_hunk_action = Some(act);
            }
            if ui.button("Clear selection").clicked() {
                ws.hunk_selection.selected_lines.clear();
            }
        });
        ui.separator();
    }

    // Per-hunk rendering — one `CollapsingHeader`-style strip per
    // hunk, with action buttons on the right and a checkbox gutter on
    // every Add/Remove line.
    ScrollArea::vertical()
        .id_salt(("working_diff_with_staging", path.display().to_string()))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (hunk_idx, hunk) in hunks.iter().enumerate() {
                render_hunk_with_controls(ui, ws, &path, side, hunk_idx, hunk);
            }
        });
}

fn render_hunk_with_controls(
    ui: &mut egui::Ui,
    ws: &mut WorkspaceState,
    path: &std::path::Path,
    side: DiffSide,
    hunk_idx: usize,
    hunk: &crate::git::Hunk,
) {
    // Hunk header strip: blue header text on the left, action buttons
    // on the right. The header row is one row high (DIFF_ROW_HEIGHT)
    // for visual consistency with the commit-diff renderer.
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(&hunk.header)
                .monospace()
                .color(Color32::from_rgb(110, 170, 220)),
        );
        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
            match side {
                DiffSide::Unstaged => {
                    if ui
                        .button(RichText::new("Discard hunk").color(Color32::from_rgb(212, 92, 92)))
                        .on_hover_text("Throw away this hunk from the working tree (cannot be undone)")
                        .clicked()
                    {
                        ws.pending_hunk_action = Some(HunkAction::DiscardHunk {
                            file: path.to_path_buf(),
                            hunk_index: hunk_idx,
                            line_indices: Vec::new(),
                        });
                    }
                    if ui
                        .button("Stage hunk")
                        .on_hover_text("Move this hunk into the index (`git apply --cached`)")
                        .clicked()
                    {
                        ws.pending_hunk_action = Some(HunkAction::StageHunk {
                            file: path.to_path_buf(),
                            hunk_index: hunk_idx,
                            line_indices: Vec::new(),
                        });
                    }
                }
                DiffSide::Staged => {
                    if ui
                        .button("Unstage hunk")
                        .on_hover_text("Move this hunk back out of the index")
                        .clicked()
                    {
                        ws.pending_hunk_action = Some(HunkAction::UnstageHunk {
                            file: path.to_path_buf(),
                            hunk_index: hunk_idx,
                            line_indices: Vec::new(),
                        });
                    }
                }
            }
        });
    });

    // Per-line rows with a checkbox gutter for Add/Remove lines.
    for (line_idx, line) in hunk.lines.iter().enumerate() {
        paint_diff_line_with_checkbox(ui, ws, hunk_idx, line_idx, line);
    }
    ui.add_space(4.0);
}

fn paint_diff_line_with_checkbox(
    ui: &mut egui::Ui,
    ws: &mut WorkspaceState,
    hunk_idx: usize,
    line_idx: usize,
    line: &DiffLine,
) {
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

    let is_staggered = matches!(line.kind, LineKind::Add | LineKind::Remove);

    let old = line
        .old_lineno
        .map(|n| format!("{n:>4}"))
        .unwrap_or_else(|| "    ".into());
    let new = line
        .new_lineno
        .map(|n| format!("{n:>4}"))
        .unwrap_or_else(|| "    ".into());
    let text = format!(" {old} {new} {prefix} {}", line.content);

    let row_height = 16.0;
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), row_height),
        egui::Sense::click(),
    );
    if bg.a() > 0 {
        ui.painter().rect_filled(rect, 0.0, bg);
    }

    // Left gutter: 18 px reserved for checkbox on stage-able rows. On
    // context / meta rows the gutter stays blank so the prefix column
    // still lines up.
    const CHECKBOX_W: f32 = 18.0;
    let mid_y = rect.center().y;

    if is_staggered {
        let selected = ws.hunk_selection.is_selected(hunk_idx, line_idx);
        let cb_rect = egui::Rect::from_min_size(
            egui::pos2(rect.min.x + 2.0, mid_y - 7.0),
            Vec2::new(14.0, 14.0),
        );
        let cb_resp = ui.interact(
            cb_rect,
            ui.id().with(("hunk_line_cb", hunk_idx, line_idx)),
            egui::Sense::click(),
        );
        let glyph = if selected { "☑" } else { "☐" };
        let col = if selected {
            ui.visuals().text_color()
        } else {
            ui.visuals().weak_text_color()
        };
        ui.painter().text(
            cb_rect.center(),
            egui::Align2::CENTER_CENTER,
            glyph,
            FontId::monospace(13.0),
            col,
        );
        // Checkbox OR whole-row click toggles — but only once per
        // click (egui surfaces both `cb_resp.clicked()` and
        // `resp.clicked()` when the pointer is inside the checkbox
        // rect, so we treat the checkbox as the authoritative hit
        // when it reports one).
        if cb_resp.clicked() {
            ws.hunk_selection.toggle_line(hunk_idx, line_idx);
        } else if resp.clicked() {
            ws.hunk_selection.toggle_line(hunk_idx, line_idx);
        }
    }

    ui.painter().text(
        rect.min + Vec2::new(CHECKBOX_W + 4.0, 2.0),
        egui::Align2::LEFT_TOP,
        text,
        FontId::monospace(12.5),
        fg,
    );
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

/// Aggregated counts for a folder — drives the "[N modified, M new]" subtitle
/// under each folder header. Grouping by `DeltaStatus` lets the user see at a
/// glance what happened in a folder without having to unfold it (especially
/// useful for commits that touch hundreds of files across a handful of dirs).
#[derive(Default, Clone, Copy)]
struct FolderStats {
    added: usize,
    modified: usize,
    deleted: usize,
    renamed: usize,
    other: usize,
}

impl FolderStats {
    fn push(&mut self, status: DeltaStatus) {
        match status {
            DeltaStatus::Added => self.added += 1,
            DeltaStatus::Modified => self.modified += 1,
            DeltaStatus::Deleted => self.deleted += 1,
            DeltaStatus::Renamed | DeltaStatus::Copied => self.renamed += 1,
            _ => self.other += 1,
        }
    }

    /// Render as "3 modified, 1 new" — skips zero buckets so the line stays
    /// scannable. Returns None when nothing interesting happened (empty folder
    /// shouldn't get a subtitle at all).
    fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.modified > 0 {
            parts.push(format!("{} modified", self.modified));
        }
        if self.added > 0 {
            parts.push(format!("{} new", self.added));
        }
        if self.deleted > 0 {
            parts.push(format!("{} deleted", self.deleted));
        }
        if self.renamed > 0 {
            parts.push(format!("{} renamed", self.renamed));
        }
        if self.other > 0 {
            parts.push(format!("{} other", self.other));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(", "))
        }
    }
}

/// Three-state flag for the folder-level staging checkbox. Working-tree tree
/// view uses this; commit-detail tree view ignores it (no staging concept).
#[derive(Clone, Copy, PartialEq, Eq)]
enum FolderStage {
    /// Every file in the folder is fully staged (and not also dirty-in-wt).
    All,
    /// Nothing in the folder is staged — pure working-tree changes.
    None,
    /// Partial — some staged, some not, or files with both index + wt diffs.
    /// Renders as "–" instead of "✓" so the user knows one more click won't
    /// do what they think.
    Mixed,
}

/// Render a folder header row: fold-triangle, folder icon, breadcrumb path
/// in monospace, and an aggregated "[N modified, M new]" subtitle. Returns
/// `(whether-the-header-was-clicked-to-toggle, checkbox-response)` so
/// callers can wire up folder-scoped actions (stage all / unstage all) and
/// expand/collapse without re-laying out the row.
///
/// We draw the row manually instead of using `egui::CollapsingHeader` because:
/// 1. We put a checkbox *inside* the header, and egui's collapsing header
///    eats clicks on anything inside its title row, making the checkbox
///    stop working as a distinct control.
/// 2. We need the subtitle line directly under the header, not indented as a
///    child — a collapsing header forces that indentation.
/// The tradeoff is that we manage the `open` flag ourselves via `ws`-side
/// state; callers pass it in and we return whether the user toggled it.
fn folder_header_row(
    ui: &mut egui::Ui,
    open: bool,
    dir_label: &str,
    stats: &FolderStats,
    stage: Option<FolderStage>,
) -> (bool, Option<egui::Response>) {
    let row_h = FILE_ROW_HEIGHT + 2.0;
    let row_w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(row_w, row_h),
        egui::Sense::click(),
    );
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 0.0, ui.visuals().faint_bg_color);
    }

    // Layout (left → right): [▸/▾] [☐/☑/–] 📁 path/in/mono  (aggregate stats)
    let mut cursor_x = rect.min.x + 4.0;
    let mid_y = rect.center().y;

    // Fold triangle.
    let tri = if open { "▾" } else { "▸" };
    let tri_galley = ui.painter().layout_no_wrap(
        tri.into(),
        FontId::monospace(12.0),
        ui.visuals().widgets.noninteractive.fg_stroke.color,
    );
    ui.painter().galley(
        egui::pos2(cursor_x, mid_y - tri_galley.size().y * 0.5),
        tri_galley,
        ui.visuals().widgets.noninteractive.fg_stroke.color,
    );
    cursor_x += 14.0;

    // Tri-state checkbox — drawn inline, with its own click rect so the
    // folder-row click (toggle open/close) and the checkbox click (stage /
    // unstage all) don't fight for the same pointer event.
    let checkbox_resp = if let Some(stage) = stage {
        let cb_rect = egui::Rect::from_min_size(
            egui::pos2(cursor_x, mid_y - 8.0),
            egui::vec2(16.0, 16.0),
        );
        let cb_resp = ui.interact(
            cb_rect,
            ui.id().with(("folder_cb", dir_label)),
            egui::Sense::click(),
        );
        let (glyph, col) = match stage {
            FolderStage::All => ("☑", ui.visuals().text_color()),
            FolderStage::None => ("☐", ui.visuals().weak_text_color()),
            FolderStage::Mixed => ("–", ui.visuals().warn_fg_color),
        };
        let g = ui.painter().layout_no_wrap(
            glyph.into(),
            FontId::monospace(14.0),
            col,
        );
        ui.painter().galley(
            egui::pos2(cb_rect.min.x, mid_y - g.size().y * 0.5),
            g,
            col,
        );
        cursor_x += 20.0;
        Some(cb_resp)
    } else {
        None
    };

    // Folder icon + breadcrumb path.
    let folder_glyph = "📁";
    let fg = ui.painter().layout_no_wrap(
        folder_glyph.into(),
        FontId::proportional(13.0),
        ui.visuals().text_color(),
    );
    ui.painter().galley(
        egui::pos2(cursor_x, mid_y - fg.size().y * 0.5),
        fg,
        ui.visuals().text_color(),
    );
    cursor_x += 20.0;

    let path_galley = ui.painter().layout_no_wrap(
        dir_label.to_string(),
        FontId::monospace(12.5),
        ui.visuals().strong_text_color(),
    );
    ui.painter().galley(
        egui::pos2(cursor_x, mid_y - path_galley.size().y * 0.5),
        path_galley.clone(),
        ui.visuals().strong_text_color(),
    );
    cursor_x += path_galley.size().x + 10.0;

    // Aggregate stats subtitle — right-aligned so it doesn't overlap the path
    // when folder names are long.
    if let Some(summary) = stats.summary() {
        let subtitle = format!("[{}]", summary);
        let sg = ui.painter().layout_no_wrap(
            subtitle,
            FontId::proportional(11.0),
            ui.visuals().weak_text_color(),
        );
        let sx = (rect.right() - sg.size().x - 6.0).max(cursor_x);
        ui.painter().galley(
            egui::pos2(sx, mid_y - sg.size().y * 0.5),
            sg,
            ui.visuals().weak_text_color(),
        );
    }

    // If the checkbox grabbed the click, the row click shouldn't also fire;
    // egui returns both responses for nested interact rects, so check the
    // checkbox response first.
    let cb_clicked = checkbox_resp
        .as_ref()
        .map(|r| r.clicked())
        .unwrap_or(false);
    let header_clicked = resp.clicked() && !cb_clicked;
    (header_clicked, checkbox_resp)
}

/// Tree view — files grouped by directory with folder headers + tri-state
/// staging checkboxes. Multi-level paths (`a/b/c`) render as a single breadcrumb row
/// rather than nested groups; flatter = easier to scan when a commit touches
/// many sibling subtrees.
///
/// For commit diffs we *don't* show a folder-level staging checkbox since
/// "stage" is meaningless for an existing commit's files.
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
                    dir.clone()
                };

                // Track open/closed state per folder-per-panel via egui's
                // persistent memory. This keeps the user's fold preferences
                // across frames without threading state through WorkspaceState
                // (we'd need `app.rs` changes for that, which is out of scope).
                let open_id = ui.id().with(("diff_tree_open", dir));
                let mut open = ui
                    .ctx()
                    .data(|d| d.get_temp::<bool>(open_id))
                    .unwrap_or(true);

                let mut stats = FolderStats::default();
                for &(_, f) in files {
                    stats.push(f.status);
                }

                // Commit-detail view: no staging checkbox.
                let (toggled, _) = folder_header_row(ui, open, &dir_label, &stats, None);
                if toggled {
                    open = !open;
                    ui.ctx().data_mut(|d| d.insert_temp(open_id, open));
                }
                if !open {
                    continue;
                }
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
                            .max_rect(rect.shrink2(egui::vec2(16.0, 0.0)))
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

/// Width of the minimap column appended to the right of the diff
/// scroll area. 14 px is narrow enough that even pane-split layouts
/// don't lose noticeable text width; at the same time it leaves room
/// for the viewport overlay + drag target.
const MINIMAP_WIDTH: f32 = 14.0;

/// Key for the one-shot pending scroll offset that the minimap writes
/// and the diff scroll area consumes on the next frame. Stored in the
/// egui data bag so multiple files can coexist without us threading a
/// per-file scroll cache through `WorkspaceState`.
fn pending_scroll_key() -> egui::Id {
    egui::Id::new("diff_minimap::pending_scroll")
}

/// Tiny toolbar button that toggles the minimap. Kept inline rather
/// than given its own widget module — the surface is one bool + one
/// tooltip, not worth the indirection yet.
fn minimap_toggle_button(ui: &mut egui::Ui, diff_prefs: &mut crate::config::DiffPrefs) {
    // Unicode glyph chosen to read as "vertical strip / overview"
    // without leaning on a raster icon pipeline. The active state
    // uses SelectableLabel so the toggle affords its state the same
    // way the file-view / diff-view toggles beside it do.
    let label = "▮";
    let hover = if diff_prefs.show_minimap {
        "Hide diff minimap"
    } else {
        "Show diff minimap"
    };
    let btn_size = egui::vec2(26.0, 22.0);
    let resp = ui
        .add_sized(
            btn_size,
            egui::SelectableLabel::new(diff_prefs.show_minimap, label),
        )
        .on_hover_text(hover);
    if resp.clicked() {
        diff_prefs.show_minimap = !diff_prefs.show_minimap;
    }
}

fn render_file_detail(
    ui: &mut egui::Ui,
    file: &FileDiff,
    image_cache: Option<&SelectedImageCache>,
    diff_prefs: &mut crate::config::DiffPrefs,
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
            //
            // While walking the hunks, we also precompute intra-line
            // word diffs for each adjacent Remove+Add pair so the
            // per-row painter stays allocation-free. `intra_line_diff`
            // returns `None` when the two lines are too dissimilar to
            // align meaningfully; in that case the row falls back to
            // the solid red/green rendering.
            let mut rows: Vec<DiffRow> = Vec::with_capacity(
                hunks.iter().map(|h| h.lines.len() + 1).sum::<usize>() + usize::from(*truncated),
            );
            let mut intra: Vec<Option<std::sync::Arc<crate::git::IntraLineDiff>>> =
                Vec::with_capacity(rows.capacity());
            for hunk in hunks {
                rows.push(DiffRow::HunkHeader(&hunk.header));
                intra.push(None);
                let lines = &hunk.lines;
                let mut i = 0;
                while i < lines.len() {
                    let line = &lines[i];
                    // Adjacent Remove+Add with matching hunk context
                    // = candidate for intra-line alignment.
                    let pair = if line.kind == LineKind::Remove && i + 1 < lines.len() {
                        let next = &lines[i + 1];
                        if next.kind == LineKind::Add {
                            crate::git::intra_line_diff(&line.content, &next.content)
                                .map(std::sync::Arc::new)
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    rows.push(DiffRow::Line(line));
                    intra.push(pair.clone());
                    if pair.is_some() {
                        // Consume the paired Add line in the same
                        // iteration so the `intra` entry lines up 1:1
                        // with `rows`.
                        let added = &lines[i + 1];
                        rows.push(DiffRow::Line(added));
                        intra.push(pair);
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            if *truncated {
                rows.push(DiffRow::Truncated);
                intra.push(None);
            }
            let total = rows.len();

            // Compute the minimap rows up-front — cheap (one pass over
            // the same data we already walked) and we need the length
            // for the viewport overlay maths anyway.
            let minimap_rows = if diff_prefs.show_minimap {
                crate::ui::diff_minimap::rows_for_file(file)
            } else {
                Vec::new()
            };
            let show_minimap = diff_prefs.show_minimap && !minimap_rows.is_empty();

            let total_y = DIFF_ROW_HEIGHT * total as f32;
            let pending_scroll = ui.data(|d| d.get_temp::<f32>(pending_scroll_key()));

            ui.horizontal(|ui| {
                let minimap_width = if show_minimap { MINIMAP_WIDTH } else { 0.0 };
                let scroll_width = (ui.available_width() - minimap_width).max(1.0);

                // Child region with a fixed width so the ScrollArea
                // doesn't shove the minimap off the right edge on
                // narrow panels.
                let scroll_rect = egui::Rect::from_min_size(
                    ui.cursor().min,
                    egui::vec2(scroll_width, ui.available_height()),
                );
                let mut scroll_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(scroll_rect)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );

                let mut scroll_area = ScrollArea::vertical()
                    .id_salt("diff_file_detail")
                    .auto_shrink([false, false]);
                if let Some(y) = pending_scroll {
                    scroll_area = scroll_area.scroll_offset(egui::vec2(0.0, y));
                }
                let output = scroll_area.show_rows(
                    &mut scroll_ui,
                    DIFF_ROW_HEIGHT,
                    total,
                    |ui, range| {
                        for i in range {
                            paint_diff_row_with_intra(ui, &rows[i], intra[i].as_deref());
                        }
                    },
                );
                // Reserve the rest of the horizontal slot so the
                // minimap lands at the cursor rather than pushing
                // through a phantom gap.
                ui.advance_cursor_after_rect(output.inner_rect);

                if show_minimap {
                    let viewport_h = output.inner_rect.height();
                    let scroll_y = output.state.offset.y;
                    if let Some(new_y) = crate::ui::diff_minimap::show(
                        ui,
                        &minimap_rows,
                        scroll_y,
                        total_y,
                        viewport_h,
                        minimap_width,
                    ) {
                        ui.data_mut(|d| d.insert_temp(pending_scroll_key(), new_y));
                    } else if pending_scroll.is_some() {
                        // One-shot: consume the pending value so the
                        // ScrollArea isn't pinned to the same offset
                        // on every subsequent frame.
                        ui.data_mut(|d| d.remove::<f32>(pending_scroll_key()));
                    }
                } else if pending_scroll.is_some() {
                    // Minimap turned off with a pending scroll in the
                    // bag — clear it so toggling it back on doesn't
                    // resurrect the stale target.
                    ui.data_mut(|d| d.remove::<f32>(pending_scroll_key()));
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

fn paint_diff_row_with_intra(
    ui: &mut egui::Ui,
    row: &DiffRow<'_>,
    intra: Option<&crate::git::IntraLineDiff>,
) {
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
        DiffRow::Line(line) => paint_diff_line_with_intra(ui, line, intra),
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

/// Paint one diff row, optionally layering intra-line word-diff
/// highlights on top of the soft row background. Passing `None` gives
/// the same solid red/green rendering the first MVP shipped with;
/// passing `Some(diff)` picks the spans for this row's kind out of the
/// `IntraLineDiff` pair and paints them as a stronger emphasis band.
fn paint_diff_line_with_intra(
    ui: &mut egui::Ui,
    line: &DiffLine,
    intra: Option<&crate::git::IntraLineDiff>,
) {
    // Each line is rendered as a full-width row with a background colour
    // and a fixed-width gutter showing old/new line numbers, then the
    // content in monospace. Word-level highlights, when available, go
    // as a second pass: the whole-line background stays soft so the
    // row still reads as "this entire line was removed / added", and
    // the RemovedWord / AddedWord spans get a stronger rectangle
    // layered on top of the line background.
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
    let prefix_text = format!(" {old} {new} {prefix} ");
    let body = line.content.as_str();
    let full_text = format!("{prefix_text}{body}");

    // Allocate a full-width row, paint background, then draw text.
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 16.0), egui::Sense::hover());
    if bg.a() > 0 {
        ui.painter().rect_filled(rect, 0.0, bg);
    }

    // Intra-line emphasis: paint a second rectangle for each span that
    // is a word-level change on this row. We measure via the monospace
    // glyph advance so the highlights align with the text grid —
    // egui's `painter().text` doesn't give us a per-span layout, so we
    // lean on the fact that we're already guaranteed to be in a
    // fixed-width font.
    if let (Some(intra), true) = (intra, matches!(line.kind, LineKind::Add | LineKind::Remove)) {
        let spans = match line.kind {
            LineKind::Remove => &intra.removed_spans,
            LineKind::Add => &intra.added_spans,
            _ => unreachable!(),
        };
        let emphasis_bg = match line.kind {
            LineKind::Remove => Color32::from_rgba_unmultiplied(235, 108, 108, 90),
            LineKind::Add => Color32::from_rgba_unmultiplied(116, 192, 136, 90),
            _ => Color32::TRANSPARENT,
        };
        let advance = monospace_advance(ui, &font);
        let base_x = rect.min.x + 4.0 + advance * prefix_text.chars().count() as f32;
        let mut cursor = 0usize;
        for span in spans {
            let (text, emphasized) = match span {
                crate::git::IntraLineSpan::Unchanged(t) => (t.as_str(), false),
                crate::git::IntraLineSpan::RemovedWord(t) => {
                    (t.as_str(), matches!(line.kind, LineKind::Remove))
                }
                crate::git::IntraLineSpan::AddedWord(t) => {
                    (t.as_str(), matches!(line.kind, LineKind::Add))
                }
            };
            let width = advance * text.chars().count() as f32;
            if emphasized && !text.is_empty() {
                let x = base_x + advance * cursor as f32;
                let stripe = egui::Rect::from_min_size(
                    egui::pos2(x, rect.min.y),
                    Vec2::new(width, rect.height()),
                );
                ui.painter().rect_filled(stripe, 0.0, emphasis_bg);
            }
            cursor += text.chars().count();
        }
    }

    ui.painter().text(
        rect.min + Vec2::new(4.0, 2.0),
        egui::Align2::LEFT_TOP,
        full_text,
        font,
        fg,
    );
}

/// Horizontal advance of one glyph in the given monospace font.
/// Cheap enough to call once per row — egui's font system memoizes
/// glyph metrics internally.
fn monospace_advance(ui: &egui::Ui, font: &FontId) -> f32 {
    ui.fonts(|fonts| fonts.glyph_width(font, 'M'))
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
