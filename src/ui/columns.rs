//! Column visibility picker for the commit graph.
//!
//! The graph shows a horizontal row per commit; users want to toggle which
//! columns are visible (Branch/Tag chips, the graph itself, message, author,
//! date, sha), plus a couple of layout modes. We keep the prefs on the
//! per-tab WorkspaceState so different repos can have different views.
//!
//! This module provides:
//!   * `ColumnPrefs` — the data type (also used by `graph.rs` to decide
//!     what to paint).
//!   * A tiny `show()` modal that renders a checklist-style popover.
//!
//! The popover is opened by the ⚙ button in the main-panel toolbar; it
//! closes on Esc or when the user clicks "Done".

use egui::{Color32, RichText};

use crate::app::{MergeFoxApp, View};

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnPrefs {
    pub show_refs: bool,    // Branch / tag chips column
    pub show_graph: bool,   // The ASCII-ish lane graph
    pub show_message: bool, // The commit summary text
    pub show_author: bool,
    pub show_date: bool,
    pub show_sha: bool,
    /// Compact-graph-column: shrinks the lane cells to half the usual LANE_WIDTH.
    pub compact_graph: bool,
    /// Smart-branch-visibility: hides branch chips on rows that already
    /// have the branch name shown by a descendant.
    pub smart_branches: bool,
    /// User-overridden column widths (drag handles in the graph view).
    /// `None` means "auto" — the graph view picks a default for that
    /// column based on content / heuristics. Persist across tab
    /// switches via WorkspaceState, but reset to None when the user
    /// hits "Reset columns to default".
    pub graph_width: Option<f32>,
    pub refs_width: Option<f32>,
    pub author_width: Option<f32>,
    pub date_width: Option<f32>,
    pub sha_width: Option<f32>,
}

impl ColumnPrefs {
    pub fn default_full() -> Self {
        Self {
            show_refs: true,
            show_graph: true,
            show_message: true,
            show_author: true,
            show_date: true,
            show_sha: false,
            compact_graph: false,
            smart_branches: true,
            graph_width: None,
            refs_width: None,
            author_width: None,
            date_width: None,
            sha_width: None,
        }
    }

    /// Compact layout — graph + message only.
    pub fn compact() -> Self {
        Self {
            show_refs: true,
            show_graph: true,
            show_message: true,
            show_author: false,
            show_date: false,
            show_sha: false,
            compact_graph: true,
            smart_branches: true,
            graph_width: None,
            refs_width: None,
            author_width: None,
            date_width: None,
            sha_width: None,
        }
    }
}

impl Default for ColumnPrefs {
    fn default() -> Self {
        Self::default_full()
    }
}

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    if !app.columns_popover_open {
        return;
    }
    let View::Workspace(tabs) = &mut app.view else {
        app.columns_popover_open = false;
        return;
    };
    let prefs = &mut tabs.current_mut().column_prefs;

    let mut open = true;
    let mut reset_default = false;
    let mut reset_compact = false;
    let mut reset_widths = false;

    egui::Window::new("⚙ Columns")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(260.0)
        .show(ctx, |ui| {
            ui.label(RichText::new("Visible columns").color(Color32::LIGHT_BLUE));
            ui.separator();
            check(ui, &mut prefs.show_refs, "Branch / Tag");
            check(ui, &mut prefs.show_graph, "Graph");
            check(ui, &mut prefs.show_message, "Commit message");
            check(ui, &mut prefs.show_author, "Author");
            check(ui, &mut prefs.show_date, "Date / Time");
            check(ui, &mut prefs.show_sha, "Sha");
            ui.separator();
            check(ui, &mut prefs.compact_graph, "Compact Graph Column");
            check(ui, &mut prefs.smart_branches, "Smart Branch Visibility");
            ui.separator();
            // Reset just the user-dragged column widths, leaving
            // visibility + compact/smart toggles alone. Useful when
            // the user overshoots a drag and wants auto-sizing back
            // without losing their show/hide preferences.
            if ui.button("Reset column widths").clicked() {
                reset_widths = true;
            }
            if ui.button("Reset columns to default layout").clicked() {
                reset_default = true;
            }
            if ui.button("Reset columns to compact layout").clicked() {
                reset_compact = true;
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Done").clicked() {
                        app_close(ui.ctx());
                    }
                });
            });
        });

    if reset_default {
        *prefs = ColumnPrefs::default_full();
    }
    if reset_compact {
        *prefs = ColumnPrefs::compact();
    }
    if reset_widths {
        prefs.graph_width = None;
        prefs.refs_width = None;
        prefs.author_width = None;
        prefs.date_width = None;
        prefs.sha_width = None;
    }
    if !open {
        app.columns_popover_open = false;
    }
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.columns_popover_open = false;
    }
}

fn check(ui: &mut egui::Ui, val: &mut bool, label: &str) {
    let prefix = if *val { "✓ " } else { "   " };
    if ui
        .selectable_label(*val, format!("{prefix}{label}"))
        .clicked()
    {
        *val = !*val;
    }
}

/// Placeholder — close logic lives in show() directly via `open` flag,
/// so this is a no-op for now. Kept so the Done button has something to
/// call in case we later want to signal through egui memory.
fn app_close(_ctx: &egui::Context) {}
