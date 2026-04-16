//! Commit graph view — virtual-scrolled, custom-painted.
//!
//! Per-row paint model (top half = above commit center, bottom half = below):
//!
//!   TOP half (from `prev.lanes_below`):
//!     * If the lane is listed in `this.incoming_lanes` → diagonal line
//!       from (prev_lane_x, top) to (my_lane_x, mid). [line terminates here]
//!     * Else → straight pass-through from (lane_x, top) to (lane_x, mid).
//!
//!   BOTTOM half (from `this.lanes_below` + `edges_out`):
//!     * For each lane present in `lanes_below` AND also present in
//!       `prev.lanes_below` AND not this commit's lane → straight
//!       pass-through (lane_x, mid) → (lane_x, bottom).
//!     * For each edge_out → diagonal from (my_lane_x, mid) to
//!       (target_lane_x, bottom). If target == my_lane, this is simply
//!       a straight line down.

use std::sync::Arc;

use egui::{Color32, FontId, Painter, Pos2, Rect, Sense, Stroke, Ui, Vec2};
use gix::ObjectId as Oid;

use crate::actions::{CommitAction, ResetMode};
use crate::git::{CommitGraph, GraphRow};
use crate::ui::columns::ColumnPrefs;

pub const ROW_HEIGHT: f32 = 24.0;
pub const LANE_WIDTH: f32 = 16.0;
pub const COMPACT_LANE_WIDTH: f32 = 10.0;
const DOT_RADIUS: f32 = 4.5;
const LINE_WIDTH: f32 = 1.8;

/// Hard ceiling on the graph column's visible width. Beyond this, the
/// column stays this wide and the user scrolls horizontally inside it
/// with mouse-wheel (or shift-wheel). Without the cap, Linux-kernel-scale
/// histories push the commit-message column far enough right that you
/// need a second monitor to read summaries.
///
/// 160 px keeps the visual weight of the graph low — in mergeFox most
/// screen real estate should go to the commit subjects, not the colour
/// ribbon on the left. Users with deep merge histories can drag the
/// column handle wider interactively (up to `GRAPH_MAX_WIDTH`), or they
/// can scroll horizontally within the fixed column.
pub const GRAPH_COLUMN_MAX_WIDTH: f32 = 160.0;

// Per-column resize bounds for the user-draggable dividers. We clamp
// aggressively: letting the user drag a column to 0 px would strand
// the row's right-hand side off-screen and make recovery annoying
// (no visible divider left to grab). Likewise a 1200 px author column
// would eat the whole row.
const GRAPH_MIN_WIDTH: f32 = 24.0;
const GRAPH_MAX_WIDTH: f32 = 600.0;
const REFS_MIN_WIDTH: f32 = 36.0;
const REFS_MAX_WIDTH: f32 = 480.0;
const META_MIN_WIDTH: f32 = 40.0;
const META_MAX_WIDTH: f32 = 320.0;
const AUTHOR_DEFAULT: f32 = 140.0;
const DATE_DEFAULT: f32 = 60.0;
const SHA_DEFAULT: f32 = 68.0;
const COLUMN_GAP: f32 = 6.0;
/// Width of the invisible hit zone for a column divider, centered on
/// the visible 1 px line. Wide enough to grab on a trackpad without
/// zooming in.
const HANDLE_HIT_WIDTH: f32 = 6.0;

/// Handle identifier — which `ColumnPrefs` width the drag mutates.
#[derive(Clone, Copy)]
enum HandleKind {
    Graph,
    Refs,
    AuthorLeft,
    DateLeft,
    ShaLeft,
}

/// Explicit per-row rects, computed once per frame from the shared
/// column widths. `message` always takes the flex remainder between
/// the left-anchored columns (graph, refs) and the right-anchored
/// ones (author, date, sha).
struct RowCells {
    graph: Rect,
    refs: Rect,
    message: Rect,
    author: Rect,
    date: Rect,
    sha: Rect,
}

struct ColumnLayout {
    show_graph: bool,
    show_refs_column: bool,
    show_message: bool,
    show_author: bool,
    show_date: bool,
    show_sha: bool,
    graph_width: f32,
    refs_width: f32,
    author_width: f32,
    date_width: f32,
    sha_width: f32,
}

impl ColumnLayout {
    fn compute(&self, rect: Rect) -> RowCells {
        let top = rect.top();
        let h = rect.height();
        let mut cursor = rect.left();

        // Left-anchored columns paint in reading order: refs first
        // (branch/tag chips at the front), then the graph lanes, then
        // flex message. Graph content is left-aligned INSIDE its own
        // column (`paint_graph_cell` positions lanes from the column's
        // left edge), so the lanes always hug the left of the graph
        // cell regardless of how wide the user has dragged it.
        let refs = if self.show_refs_column {
            let r = Rect::from_min_size(Pos2::new(cursor, top), Vec2::new(self.refs_width, h));
            cursor += self.refs_width + COLUMN_GAP;
            r
        } else {
            Rect::NOTHING
        };
        let graph = if self.show_graph {
            let r = Rect::from_min_size(Pos2::new(cursor, top), Vec2::new(self.graph_width, h));
            cursor += self.graph_width + COLUMN_GAP;
            r
        } else {
            Rect::NOTHING
        };

        // Right-anchored columns: compute rects from rect.right() inward.
        let mut right_cursor = rect.right();
        let sha = if self.show_sha {
            right_cursor -= self.sha_width;
            let r = Rect::from_min_size(Pos2::new(right_cursor, top), Vec2::new(self.sha_width, h));
            right_cursor -= COLUMN_GAP;
            r
        } else {
            Rect::NOTHING
        };
        let date = if self.show_date {
            right_cursor -= self.date_width;
            let r =
                Rect::from_min_size(Pos2::new(right_cursor, top), Vec2::new(self.date_width, h));
            right_cursor -= COLUMN_GAP;
            r
        } else {
            Rect::NOTHING
        };
        let author = if self.show_author {
            right_cursor -= self.author_width;
            let r = Rect::from_min_size(
                Pos2::new(right_cursor, top),
                Vec2::new(self.author_width, h),
            );
            right_cursor -= COLUMN_GAP;
            r
        } else {
            Rect::NOTHING
        };

        // Message spans whatever's left between the two clusters. If
        // the right-anchored columns overflow into the left cluster
        // (user dragged too aggressively), message collapses to 0 px
        // rather than flipping.
        let message = if self.show_message {
            let start = cursor;
            let end = right_cursor.max(cursor);
            Rect::from_min_size(Pos2::new(start, top), Vec2::new((end - start).max(0.0), h))
        } else {
            Rect::NOTHING
        };

        RowCells {
            graph,
            refs,
            message,
            author,
            date,
            sha,
        }
    }
}

pub struct GraphView {
    pub graph: Arc<CommitGraph>,
    pub selected_row: Option<usize>,
    ref_column_width: f32,
    /// Horizontal scroll offset (px) within the fixed-width graph column.
    /// Clamped each frame to `[0, desired_graph_width - visible_width]`
    /// so the user can't drag past the end. Kept on the view so it
    /// persists across frames / scroll events.
    graph_scroll_x: f32,
}

#[derive(Default)]
pub struct GraphInteraction {
    pub clicked: Option<usize>,
    pub action: Option<CommitAction>,
    pub clear_commit_selection: bool,
    pub open_commit: bool,
}

impl GraphView {
    pub fn new(graph: Arc<CommitGraph>) -> Self {
        Self {
            ref_column_width: estimate_ref_column_width(&graph),
            graph,
            selected_row: None,
            graph_scroll_x: 0.0,
        }
    }

    /// Render the graph. `head_oid` is used to decide commit-context
    /// (HEAD vs past vs branch-tip) when building the right-click menu.
    /// `prefs` controls column visibility + widths; drag handles mutate
    /// it in place.
    ///
    /// Column order is Sourcetree-style: the graph is anchored to the
    /// far left so it stays put regardless of how wide the refs column
    /// grows. The commit-message column absorbs any flex; everything
    /// else has an explicit (user-draggable) width.
    ///
    ///   `[ graph | refs | message (flex) | author | date | sha ]`
    ///
    /// If `working_entries` is provided and not empty, renders a "Working Tree"
    /// virtual row as the first row above the actual commits.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        head_oid: Option<Oid>,
        prefs: &mut ColumnPrefs,
        working_entries: Option<&[crate::git::StatusEntry]>,
        working_selected: &mut bool,
        _working_expanded: &mut bool,
    ) -> GraphInteraction {
        let row_count = self.graph.rows.len();
        if row_count == 0 && working_entries.map(|e| e.is_empty()).unwrap_or(true) {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.weak("No commits in this scope.");
            });
            return GraphInteraction::default();
        }

        let working_summary = working_entries.map(summarize_working_tree);
        let has_working_changes = working_summary
            .as_ref()
            .map(WorkingTreeSummary::has_changes)
            .unwrap_or(false);

        let lane_w = if prefs.compact_graph {
            COMPACT_LANE_WIDTH
        } else {
            LANE_WIDTH
        };
        // The *desired* width — how wide the graph would be if we let
        // it take whatever it wanted — is still bounded by
        // `MAX_GRAPH_LANES` to stop pathological barcode rendering,
        // but we only show up to `graph_width` (user-sized) of it and
        // let the user scroll horizontally for the rest.
        let visible_max_lane = self.graph.max_lane.min(crate::git::graph::MAX_GRAPH_LANES);
        let desired_graph_width = if prefs.show_graph {
            (visible_max_lane as f32 + 1.5) * lane_w
        } else {
            0.0
        };
        let graph_width = if prefs.show_graph {
            prefs
                .graph_width
                .unwrap_or_else(|| desired_graph_width.min(GRAPH_COLUMN_MAX_WIDTH))
                .clamp(GRAPH_MIN_WIDTH, GRAPH_MAX_WIDTH)
        } else {
            0.0
        };
        // Clamp previous scroll offset in case the graph got narrower
        // (e.g. user switched to compact mode or the graph rebuilt
        // with fewer lanes). Always leave at least the visible window
        // in view.
        let max_scroll_x = (desired_graph_width - graph_width).max(0.0);
        if self.graph_scroll_x > max_scroll_x {
            self.graph_scroll_x = max_scroll_x;
        }
        if self.graph_scroll_x < 0.0 {
            self.graph_scroll_x = 0.0;
        }
        // refs column: user-sized if prefs.show_refs; collapses to a
        // narrow strip (enough for the HEAD chip) if refs hidden but a
        // HEAD exists; 0 otherwise.
        let refs_width = if prefs.show_refs {
            prefs
                .refs_width
                .unwrap_or(self.ref_column_width)
                .clamp(REFS_MIN_WIDTH, REFS_MAX_WIDTH)
        } else if head_oid.is_some() {
            46.0
        } else {
            0.0
        };
        let show_refs_column = refs_width > 0.0;

        let author_width = prefs
            .author_width
            .unwrap_or(AUTHOR_DEFAULT)
            .clamp(META_MIN_WIDTH, META_MAX_WIDTH);
        let date_width = prefs
            .date_width
            .unwrap_or(DATE_DEFAULT)
            .clamp(META_MIN_WIDTH, META_MAX_WIDTH);
        let sha_width = prefs
            .sha_width
            .unwrap_or(SHA_DEFAULT)
            .clamp(META_MIN_WIDTH, META_MAX_WIDTH);

        let layout = ColumnLayout {
            show_graph: prefs.show_graph,
            show_refs_column,
            show_message: prefs.show_message,
            show_author: prefs.show_author,
            show_date: prefs.show_date,
            show_sha: prefs.show_sha,
            graph_width,
            refs_width,
            author_width,
            date_width,
            sha_width,
        };

        let mut out = GraphInteraction::default();

        // Fixed header strip above the virtualized rows. Stays put while
        // the user scrolls, matches the ColumnLayout so each title sits
        // dead-centre over its column. Kept cheap: a single allocate +
        // per-column label, no interaction.
        render_column_header(ui, &layout, self.graph_scroll_x);

        // Working Tree virtual row (fixed at top, above scrollable commits)
        if has_working_changes {
            if let (Some(entries), Some(summary)) = (working_entries, working_summary.as_ref()) {
                let top_lane = self.graph.rows.first().map(|r| r.lane).unwrap_or(0);
                render_working_tree_row(
                    ui,
                    &layout,
                    lane_w,
                    self.graph_scroll_x,
                    entries,
                    summary,
                    top_lane,
                    working_selected,
                    &mut out,
                );
                if out.clear_commit_selection {
                    self.selected_row = None;
                }
            }
        }

        let scroll_output = egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show_rows(ui, ROW_HEIGHT, row_count, |ui, range| {
                for idx in range {
                    let (rect, resp) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), ROW_HEIGHT),
                        Sense::click_and_drag(),
                    );

                    // Selection background
                    if self.selected_row == Some(idx) {
                        ui.painter().rect_filled(
                            rect,
                            0.0,
                            ui.visuals()
                                .selection
                                .bg_fill
                                .gamma_multiply(if ui.visuals().dark_mode { 0.42 } else { 0.18 }),
                        );
                    } else if resp.hovered() {
                        ui.painter().rect_filled(
                            rect,
                            0.0,
                            ui.visuals().faint_bg_color.gamma_multiply(1.1),
                        );
                    }

                    if resp.clicked() {
                        self.selected_row = Some(idx);
                        out.clicked = Some(idx);
                    }

                    let row = &self.graph.rows[idx];
                    let prev = idx.checked_sub(1).map(|p| &self.graph.rows[p]);

                    let is_head = head_oid == Some(row.oid);
                    resp.context_menu(|ui| {
                        if let Some(action) = render_commit_menu(ui, row, is_head) {
                            out.action = Some(action);
                        }
                    });

                    let cells = layout.compute(rect);

                    if prefs.show_graph {
                        // Paint the graph in a CLIPPED sub-painter so
                        // lines never bleed into the adjacent refs /
                        // message columns, even when
                        // `desired_graph_width` exceeds the fixed
                        // visible `graph_width`.
                        let clipped = ui.painter().with_clip_rect(cells.graph);
                        paint_graph_cell(
                            &clipped,
                            cells.graph,
                            row,
                            prev,
                            (idx == 0 && has_working_changes).then_some(row.lane),
                            lane_w,
                            self.graph_scroll_x,
                        );
                    }
                    if show_refs_column {
                        paint_refs_cell(ui, cells.refs, row, is_head, prefs);
                    }
                    if prefs.show_message {
                        paint_message_cell(ui, cells.message, row);
                    }
                    if prefs.show_author {
                        paint_author_cell(ui, cells.author, row);
                    }
                    if prefs.show_date {
                        paint_date_cell(ui, cells.date, row);
                    }
                    if prefs.show_sha {
                        paint_sha_cell(ui, cells.sha, row);
                    }
                }
            });

        let inner_rect = scroll_output.inner_rect;

        // Column divider handles. We compute the x-position of each
        // boundary from the same ColumnLayout used for row painting —
        // anchored to `inner_rect` so a visible vertical scrollbar
        // doesn't throw the handle hit zones out of alignment with the
        // actual row content. Each handle hosts an invisible
        // drag-sensitive strip centered on the boundary; the visible
        // 1 px line spans the full scroll viewport height.
        let sample_row =
            Rect::from_min_size(inner_rect.min, Vec2::new(inner_rect.width(), ROW_HEIGHT));
        let sample_cells = layout.compute(sample_row);
        let mut handles: Vec<(f32, HandleKind)> = Vec::new();
        if show_refs_column && prefs.show_refs {
            handles.push((
                sample_cells.refs.right() + COLUMN_GAP * 0.5,
                HandleKind::Refs,
            ));
        }
        if prefs.show_graph {
            handles.push((
                sample_cells.graph.right() + COLUMN_GAP * 0.5,
                HandleKind::Graph,
            ));
        }
        if prefs.show_author {
            handles.push((
                sample_cells.author.left() - COLUMN_GAP * 0.5,
                HandleKind::AuthorLeft,
            ));
        }
        if prefs.show_date {
            handles.push((
                sample_cells.date.left() - COLUMN_GAP * 0.5,
                HandleKind::DateLeft,
            ));
        }
        if prefs.show_sha {
            handles.push((
                sample_cells.sha.left() - COLUMN_GAP * 0.5,
                HandleKind::ShaLeft,
            ));
        }
        for (x, kind) in handles {
            let hit = Rect::from_min_max(
                Pos2::new(x - HANDLE_HIT_WIDTH * 0.5, inner_rect.top()),
                Pos2::new(x + HANDLE_HIT_WIDTH * 0.5, inner_rect.bottom()),
            );
            let id = ui.make_persistent_id(("graph_col_handle", kind as u8));
            let resp = ui.interact(hit, id, Sense::drag());
            if resp.hovered() || resp.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
            let line_color = if resp.dragged() {
                ui.visuals().selection.bg_fill
            } else if resp.hovered() {
                ui.visuals().selection.bg_fill.gamma_multiply(0.75)
            } else {
                ui.visuals()
                    .widgets
                    .noninteractive
                    .bg_stroke
                    .color
                    .gamma_multiply(0.5)
            };
            ui.painter().vline(
                x,
                inner_rect.top()..=inner_rect.bottom(),
                Stroke::new(1.0, line_color),
            );
            let delta = resp.drag_delta().x;
            if delta != 0.0 {
                match kind {
                    HandleKind::Graph => {
                        let cur = prefs.graph_width.unwrap_or(graph_width);
                        prefs.graph_width =
                            Some((cur + delta).clamp(GRAPH_MIN_WIDTH, GRAPH_MAX_WIDTH));
                    }
                    HandleKind::Refs => {
                        let cur = prefs.refs_width.unwrap_or(refs_width);
                        prefs.refs_width =
                            Some((cur + delta).clamp(REFS_MIN_WIDTH, REFS_MAX_WIDTH));
                    }
                    HandleKind::AuthorLeft => {
                        // Left edge of a right-anchored column: dragging
                        // the handle LEFT (negative delta) GROWS the
                        // column; RIGHT (positive delta) shrinks it.
                        let cur = prefs.author_width.unwrap_or(author_width);
                        prefs.author_width =
                            Some((cur - delta).clamp(META_MIN_WIDTH, META_MAX_WIDTH));
                    }
                    HandleKind::DateLeft => {
                        let cur = prefs.date_width.unwrap_or(date_width);
                        prefs.date_width =
                            Some((cur - delta).clamp(META_MIN_WIDTH, META_MAX_WIDTH));
                    }
                    HandleKind::ShaLeft => {
                        let cur = prefs.sha_width.unwrap_or(sha_width);
                        prefs.sha_width = Some((cur - delta).clamp(META_MIN_WIDTH, META_MAX_WIDTH));
                    }
                }
            }
        }

        // Horizontal-scroll input for the graph column: if the cursor
        // is over the graph area AND the graph is wider than its
        // window, consume scroll_delta.x (natural trackpad
        // side-scroll) and any vertical wheel while Shift is held
        // (wheel-scroll convention).
        if prefs.show_graph && max_scroll_x > 0.0 {
            let ptr = ui.ctx().input(|i| i.pointer.hover_pos());
            // Graph column starts AFTER the refs column (refs is the
            // leftmost column), so offset the hit band by the refs
            // cluster width.
            let graph_x_offset = if show_refs_column {
                refs_width + COLUMN_GAP
            } else {
                0.0
            };
            let graph_band = Rect::from_min_size(
                Pos2::new(inner_rect.left() + graph_x_offset, inner_rect.top()),
                Vec2::new(graph_width, inner_rect.height()),
            );
            if ptr.map(|p| graph_band.contains(p)).unwrap_or(false) {
                let (scroll_x, shift_wheel) = ui.ctx().input(|i| {
                    let s = i.smooth_scroll_delta;
                    let wheel = if i.modifiers.shift { s.y } else { 0.0 };
                    (s.x, wheel)
                });
                let delta = scroll_x + shift_wheel;
                if delta != 0.0 {
                    self.graph_scroll_x = (self.graph_scroll_x - delta).clamp(0.0, max_scroll_x);
                    ui.ctx().request_repaint();
                }
            }
        }

        out
    }
}

fn estimate_ref_column_width(graph: &CommitGraph) -> f32 {
    const CHIP_PADDING: f32 = 12.0;
    const CHIP_SPACING: f32 = 4.0;
    const FONT_WIDTH: f32 = 6.7;
    const HEAD_WIDTH: f32 = 42.0;

    graph
        .rows
        .iter()
        .map(|row| {
            let refs_width = row
                .refs
                .iter()
                .map(|r| r.short.len() as f32 * FONT_WIDTH + CHIP_PADDING)
                .sum::<f32>();
            let spacing = row.refs.len().saturating_sub(1) as f32 * CHIP_SPACING;
            (refs_width + spacing).max(HEAD_WIDTH)
        })
        .fold(HEAD_WIDTH, f32::max)
}

/// Build the right-click menu for a commit based on its context:
///   - Past commit (not HEAD, no branch pointing here)
///   - HEAD commit
///   - Branch tip (one or more local branches point here; may also be HEAD)
///
/// Returns `Some(action)` when the user picks a menu item.
fn render_commit_menu(ui: &mut Ui, row: &GraphRow, is_head: bool) -> Option<CommitAction> {
    let mut action: Option<CommitAction> = None;
    use crate::git::graph::RefKind;
    let local_branches: Vec<&str> = row
        .refs
        .iter()
        .filter(|r| r.kind == RefKind::LocalBranch)
        .map(|r| r.short.as_ref())
        .collect();
    let remote_branches: Vec<&str> = row
        .refs
        .iter()
        .filter(|r| r.kind == RefKind::RemoteBranch)
        .map(|r| r.short.as_ref())
        .collect();
    let _ = remote_branches; // reserved for future context menu items

    // ---- branch-tip actions ----
    if !local_branches.is_empty() {
        for b in &local_branches {
            if ui.button(format!("Pull '{b}' (ff if possible)")).clicked() {
                action = Some(CommitAction::Pull {
                    branch: (*b).to_string(),
                });
                ui.close_menu();
            }
            if ui.button(format!("Push '{b}'")).clicked() {
                action = Some(CommitAction::Push {
                    branch: (*b).to_string(),
                    force: false,
                });
                ui.close_menu();
            }
            if ui.button(format!("Set upstream for '{b}'…")).clicked() {
                action = Some(CommitAction::SetUpstreamPrompt {
                    branch: (*b).to_string(),
                });
                ui.close_menu();
            }
        }
        ui.separator();
    }

    // ---- core actions (always) ----
    if ui.button("Checkout this commit").clicked() {
        action = Some(CommitAction::Checkout(row.oid));
        ui.close_menu();
    }
    if ui.button("Create worktree from commit…").clicked() {
        action = Some(CommitAction::CreateWorktreePrompt(row.oid));
        ui.close_menu();
    }
    if ui.button("Create branch here…").clicked() {
        action = Some(CommitAction::CreateBranchPrompt(row.oid));
        ui.close_menu();
    }
    if ui.button("Cherry-pick commit").clicked() {
        action = Some(CommitAction::CherryPick(row.oid));
        ui.close_menu();
    }

    // ---- reset (per local branch) ----
    if !local_branches.is_empty() {
        for b in &local_branches {
            ui.menu_button(format!("Reset '{b}' to this commit"), |ui| {
                if ui.button("Soft  — keep working copy and index").clicked() {
                    action = Some(CommitAction::Reset {
                        branch: (*b).to_string(),
                        mode: ResetMode::Soft,
                        target: row.oid,
                    });
                    ui.close_menu();
                }
                if ui
                    .button("Mixed — keep working copy, reset index")
                    .clicked()
                {
                    action = Some(CommitAction::Reset {
                        branch: (*b).to_string(),
                        mode: ResetMode::Mixed,
                        target: row.oid,
                    });
                    ui.close_menu();
                }
                if ui
                    .button(
                        egui::RichText::new("Hard  — discard all changes")
                            .color(Color32::LIGHT_RED),
                    )
                    .clicked()
                {
                    action = Some(CommitAction::Reset {
                        branch: (*b).to_string(),
                        mode: ResetMode::Hard,
                        target: row.oid,
                    });
                    ui.close_menu();
                }
            });
        }
    }

    if ui.button("Revert commit").clicked() {
        action = Some(CommitAction::Revert(row.oid));
        ui.close_menu();
    }

    // ---- HEAD-specific ----
    if is_head {
        ui.separator();
        if ui.button("Edit commit message (amend)").clicked() {
            action = Some(CommitAction::AmendMessagePrompt);
            ui.close_menu();
        }
        if ui.button("Drop commit").clicked() {
            action = Some(CommitAction::DropCommitPrompt(row.oid));
            ui.close_menu();
        }
        if ui.button("Move commit up").clicked() {
            action = Some(CommitAction::MoveCommitUp(row.oid));
            ui.close_menu();
        }
        if ui.button("Move commit down").clicked() {
            action = Some(CommitAction::MoveCommitDown(row.oid));
            ui.close_menu();
        }
    }

    // ---- branch tip: destructive ops ----
    if !local_branches.is_empty() {
        ui.separator();
        for b in &local_branches {
            if ui.button(format!("Rename '{b}'…")).clicked() {
                action = Some(CommitAction::RenameBranchPrompt {
                    from: (*b).to_string(),
                });
                ui.close_menu();
            }
            if ui
                .button(egui::RichText::new(format!("Delete '{b}'")).color(Color32::LIGHT_RED))
                .clicked()
            {
                action = Some(CommitAction::DeleteBranchPrompt {
                    name: (*b).to_string(),
                    is_remote: false,
                });
                ui.close_menu();
            }
        }
    }
    for rb in row.refs.iter().filter(|r| r.kind == RefKind::RemoteBranch) {
        let name = rb.short.as_ref();
        if ui
            .button(
                egui::RichText::new(format!("Delete remote '{name}'")).color(Color32::LIGHT_RED),
            )
            .clicked()
        {
            action = Some(CommitAction::DeleteBranchPrompt {
                name: name.to_string(),
                is_remote: true,
            });
            ui.close_menu();
        }
    }

    // ---- copy / tag ----
    ui.separator();
    if ui.button("Copy SHA").clicked() {
        action = Some(CommitAction::CopySha(row.oid));
        ui.close_menu();
    }
    if ui.button("Copy short SHA").clicked() {
        action = Some(CommitAction::CopyShortSha(row.oid));
        ui.close_menu();
    }
    if ui.button("Create tag here…").clicked() {
        action = Some(CommitAction::CreateTagPrompt {
            at: row.oid,
            annotated: false,
        });
        ui.close_menu();
    }
    if ui.button("Create annotated tag here…").clicked() {
        action = Some(CommitAction::CreateTagPrompt {
            at: row.oid,
            annotated: true,
        });
        ui.close_menu();
    }

    action
}

fn paint_graph_cell(
    painter: &Painter,
    rect: Rect,
    row: &GraphRow,
    prev: Option<&GraphRow>,
    top_connector_lane: Option<u16>,
    lane_w: f32,
    scroll_x: f32,
) {
    let mid_y = rect.center().y;
    // Collapse any lane index above the cap onto the last visible lane
    // (the "overflow" lane). This keeps grotesquely wide histories —
    // think Linux-kernel subtree merges — visually bounded. Correctness
    // of clicks / selection is preserved because we still paint the
    // commit dot at the commit's *actual* oid, just in a compressed
    // column. `scroll_x` shifts the lane paint position so the user can
    // scrub horizontally through a graph wider than the fixed column.
    let cap = crate::git::graph::MAX_GRAPH_LANES;
    let lane_x = |l: u16| {
        let clamped = l.min(cap);
        rect.left() + (clamped as f32 + 0.5) * lane_w - scroll_x
    };

    // ---- TOP HALF ----
    if let Some(prev) = prev {
        for lane in prev.lanes_below.iter().copied() {
            let x = lane_x(lane);
            let stroke = Stroke::new(LINE_WIDTH, lane_color(lane));
            if row.incoming_lanes.binary_search(&lane).is_ok() {
                // Terminates at this commit → curve into my lane.
                draw_lane_curve(
                    painter,
                    Pos2::new(x, rect.top()),
                    Pos2::new(lane_x(row.lane), mid_y),
                    stroke,
                );
            } else {
                painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, mid_y)], stroke);
            }
        }
    } else if let Some(lane) = top_connector_lane {
        let x = lane_x(lane);
        let stroke = Stroke::new(LINE_WIDTH, lane_color(lane));
        painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, mid_y)], stroke);
    }

    // ---- BOTTOM HALF ----
    // Pass-through lines: lanes active in both prev and current, not this commit's lane.
    let empty: &[u16] = &[];
    let prev_lanes: &[u16] = prev.map(|p| p.lanes_below.as_ref()).unwrap_or(empty);
    for lane in row.lanes_below.iter().copied() {
        if lane == row.lane {
            continue;
        }
        if prev_lanes.binary_search(&lane).is_err() {
            continue;
        }
        let x = lane_x(lane);
        let stroke = Stroke::new(LINE_WIDTH, lane_color(lane));
        painter.line_segment([Pos2::new(x, mid_y), Pos2::new(x, rect.bottom())], stroke);
    }

    // Outgoing edges: from (my lane, mid) → curve to (parent lane, bottom).
    for target in &row.edges_out {
        let x_end = lane_x(*target);
        let stroke = Stroke::new(LINE_WIDTH, lane_color(*target));
        draw_lane_curve(
            painter,
            Pos2::new(lane_x(row.lane), mid_y),
            Pos2::new(x_end, rect.bottom()),
            stroke,
        );
    }

    // Commit dot
    painter.circle_filled(
        Pos2::new(lane_x(row.lane), mid_y),
        DOT_RADIUS,
        lane_color(row.lane),
    );
    painter.circle_stroke(
        Pos2::new(lane_x(row.lane), mid_y),
        DOT_RADIUS,
        Stroke::new(1.0, Color32::from_black_alpha(80)),
    );
}

/// Header row titles, rendered once above the scrollable list.
///
/// We deliberately DON'T use real `Label` widgets for the titles — those
/// would sense clicks (and defeat the "click on any column to select the
/// row" intent we already fixed for data rows). Instead we paint the
/// galleys directly, which is zero-interaction by construction.
fn render_column_header(ui: &mut Ui, layout: &ColumnLayout, scroll_x: f32) {
    const HEADER_HEIGHT: f32 = 22.0;
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), HEADER_HEIGHT),
        Sense::hover(),
    );
    // Faint background so the header reads as "pinned" vs the scrolling
    // body below it.
    let bg = ui.visuals().faint_bg_color;
    ui.painter().rect_filled(rect, 0.0, bg);
    ui.painter().line_segment(
        [
            egui::pos2(rect.left(), rect.bottom() - 0.5),
            egui::pos2(rect.right(), rect.bottom() - 0.5),
        ],
        Stroke::new(1.0, ui.visuals().weak_text_color()),
    );

    let cells = layout.compute(rect);
    let font = FontId::proportional(11.0);
    let color = ui.visuals().weak_text_color();

    let mut draw_title = |column_rect: Rect, text: &str, align_left: bool| {
        if column_rect.width() <= 0.0 {
            return;
        }
        // Clip each title to its cell so it never bleeds onto the
        // neighbour (matches the row-paint strategy).
        let painter = ui.painter().with_clip_rect(column_rect);
        let galley = painter.layout_no_wrap(text.to_string(), font.clone(), color);
        let x = if align_left {
            column_rect.left() + 4.0
        } else {
            column_rect.center().x - galley.size().x * 0.5
        };
        let y = column_rect.center().y - galley.size().y * 0.5;
        painter.galley(egui::pos2(x, y), galley, color);
    };

    if layout.show_refs_column {
        draw_title(cells.refs, "Branch", true);
    }
    if layout.show_graph {
        // Graph column scrolls horizontally independently; shift the
        // title by `scroll_x` so it tracks if the user scrubs a wide
        // graph. For the common case `scroll_x == 0` this is a no-op.
        let r = cells.graph.translate(egui::vec2(-scroll_x, 0.0));
        draw_title(r, "Graph", false);
    }
    if layout.show_message {
        draw_title(cells.message, "Message", true);
    }
    if layout.show_author {
        draw_title(cells.author, "Author", true);
    }
    if layout.show_date {
        draw_title(cells.date, "Date", true);
    }
    if layout.show_sha {
        draw_title(cells.sha, "SHA", true);
    }
}

fn paint_refs_cell(ui: &mut Ui, rect: Rect, row: &GraphRow, is_head: bool, prefs: &ColumnPrefs) {
    if !is_head && !prefs.show_refs {
        return;
    }

    // Clip the child UI to the cell rect so chips that exceed the
    // column width are truncated rather than bleeding into the
    // adjacent message / graph column.
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(rect);

    if is_head {
        let fg = Color32::from_rgb(255, 220, 120);
        let galley =
            child
                .painter()
                .layout_no_wrap("HEAD".to_string(), FontId::monospace(11.0), fg);
        let pad = Vec2::new(5.0, 2.0);
        let size = galley.size() + pad * 2.0;
        let (chip_rect, _) = child.allocate_exact_size(size, Sense::hover());
        child
            .painter()
            .rect_stroke(chip_rect, 3.0, Stroke::new(1.0, fg));
        child.painter().galley(chip_rect.min + pad, galley, fg);
        child.add_space(4.0);
    }

    if !prefs.show_refs {
        return;
    }

    use crate::git::graph::RefKind;
    const LOCAL_BG: Color32 = Color32::from_rgb(60, 130, 80);
    const REMOTE_BG: Color32 = Color32::from_rgb(70, 100, 155);
    const TAG_BG: Color32 = Color32::from_rgb(160, 120, 50);
    const TAG_FG: Color32 = Color32::from_rgb(255, 240, 200);

    // ── Merge matching local + remote refs into gradient chips ──
    //
    // When `main` (local) and `origin/main` (remote) both point at the
    // same commit, render ONE chip with a left-green → right-blue
    // gradient instead of two separate chips. This makes "synced with
    // remote" visible at a glance without eating twice the horizontal
    // space.
    //
    // We collect local branches first, then for each check if a
    // matching remote exists. Matched remotes are marked so they
    // aren't rendered again individually.
    let mut consumed_remote: Vec<bool> = vec![false; row.refs.len()];

    for (i, label) in row.refs.iter().enumerate() {
        if label.kind != RefKind::LocalBranch {
            continue;
        }
        // Find a matching remote: `<anything>/<local_name>`.
        let local_name = label.short.as_ref();
        let matching_remote = row.refs.iter().enumerate().find(|(j, r)| {
            r.kind == RefKind::RemoteBranch
                && !consumed_remote[*j]
                && r.short
                    .rsplit('/')
                    .next()
                    .map(|tail| tail == local_name)
                    .unwrap_or(false)
        });

        if let Some((j, remote_label)) = matching_remote {
            consumed_remote[j] = true;
            let remote_prefix = remote_label.short.split('/').next().unwrap_or("remote");
            let font = FontId::monospace(11.0);
            let pad = Vec2::new(6.0, 2.0);

            // Measure each half's text independently so each arrow
            // sits centered inside its own colour zone.
            let left_text = format!("← {local_name}");
            let right_text = format!("{remote_prefix} →");
            let left_galley =
                child
                    .painter()
                    .layout_no_wrap(left_text, font.clone(), Color32::WHITE);
            let right_galley = child
                .painter()
                .layout_no_wrap(right_text, font, Color32::WHITE);
            let left_w = left_galley.size().x + pad.x * 2.0;
            let right_w = right_galley.size().x + pad.x * 2.0;
            let h = left_galley.size().y.max(right_galley.size().y) + pad.y * 2.0;
            let total_w = left_w + right_w;
            let (chip_rect, _) = child.allocate_exact_size(Vec2::new(total_w, h), Sense::hover());
            // Left zone: local green with left-rounded corners.
            let left_rect = Rect::from_min_size(chip_rect.min, Vec2::new(left_w, h));
            child.painter().rect_filled(
                left_rect,
                egui::Rounding {
                    nw: 3.0,
                    sw: 3.0,
                    ne: 0.0,
                    se: 0.0,
                },
                LOCAL_BG,
            );
            // Center the left galley inside the left zone.
            child.painter().galley(
                egui::pos2(
                    left_rect.center().x - left_galley.size().x * 0.5,
                    left_rect.center().y - left_galley.size().y * 0.5,
                ),
                left_galley,
                Color32::WHITE,
            );
            // Right zone: remote blue with right-rounded corners.
            let right_rect = Rect::from_min_size(
                egui::pos2(chip_rect.min.x + left_w, chip_rect.min.y),
                Vec2::new(right_w, h),
            );
            child.painter().rect_filled(
                right_rect,
                egui::Rounding {
                    nw: 0.0,
                    sw: 0.0,
                    ne: 3.0,
                    se: 3.0,
                },
                REMOTE_BG,
            );
            child.painter().galley(
                egui::pos2(
                    right_rect.center().x - right_galley.size().x * 0.5,
                    right_rect.center().y - right_galley.size().y * 0.5,
                ),
                right_galley,
                Color32::WHITE,
            );
            child.add_space(4.0);
        } else {
            // Local-only branch (no matching remote).
            paint_ref_chip(&mut child, local_name, LOCAL_BG, Color32::WHITE);
        }
    }

    // Render remaining unmatched remote branches.
    for (j, label) in row.refs.iter().enumerate() {
        if label.kind == RefKind::RemoteBranch && !consumed_remote[j] {
            paint_ref_chip(
                &mut child,
                label.short.as_ref(),
                REMOTE_BG,
                Color32::from_rgb(210, 220, 240),
            );
        }
    }

    // Render tags.
    for label in row.refs.iter() {
        if label.kind == RefKind::Tag {
            paint_ref_chip(&mut child, label.short.as_ref(), TAG_BG, TAG_FG);
        }
    }
}

/// Paint a single ref chip (used for unmerged / standalone refs).
fn paint_ref_chip(ui: &mut egui::Ui, text: &str, bg: Color32, fg: Color32) {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), FontId::monospace(11.0), fg);
    let pad = Vec2::new(6.0, 2.0);
    let size = galley.size() + pad * 2.0;
    let (chip_rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(chip_rect, 3.0, bg);
    ui.painter().galley(chip_rect.min + pad, galley, fg);
    ui.add_space(4.0);
}

fn paint_message_cell(ui: &mut Ui, rect: Rect, row: &GraphRow) {
    if rect.width() <= 0.0 {
        return;
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    // `.selectable(false)` — egui 0.29 Labels default to text-selectable
    // (for copy-on-drag). That also means they consume clicks: clicking
    // anywhere on the label text swallowed the row-select event, so users
    // had to aim at the empty gap between summary and author to select a
    // commit. These cells should never "own" a click; the whole row is
    // the click target.
    child.add(
        egui::Label::new(row.summary.as_ref())
            .truncate()
            .selectable(false),
    );
}

fn paint_author_cell(ui: &mut Ui, rect: Rect, row: &GraphRow) {
    if rect.width() <= 0.0 {
        return;
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );

    // Author avatar — GitHub-style 5×5 symmetric identicon generated
    // locally from the author's email. Set MERGEFOX_NO_AVATARS=1 to
    // skip — useful for perf A/B and for users who find the identicons
    // visually noisy.
    let skip_avatar = std::env::var("MERGEFOX_NO_AVATARS").is_ok();
    if !skip_avatar {
        paint_author_avatar(&mut child, row);
        child.add_space(4.0);
    }
    child.add(
        egui::Label::new(egui::RichText::new(row.author.as_ref()).weak())
            .truncate()
            .selectable(false),
    );
}

/// Draw a small identicon in the row's leading gutter.
///
/// The pattern is the familiar GitHub 5×5 layout: we only decide the left
/// 3 columns from hash bits, then mirror to columns 4 and 5. That gives
/// the visually symmetric look, and we render the whole thing as 25 tiny
/// rectangles with egui's painter — cheap enough to run every frame.
fn paint_author_avatar(ui: &mut egui::Ui, row: &GraphRow) {
    const AVATAR_SIZE: f32 = 18.0;
    const GRID: usize = 5;

    let (rect, _resp) =
        ui.allocate_exact_size(Vec2::new(AVATAR_SIZE, AVATAR_SIZE), egui::Sense::hover());

    let key: &str = if !row.author_email.is_empty() {
        row.author_email.as_ref()
    } else {
        row.author.as_ref()
    };

    // Hash the key into 128 bits of deterministic-but-well-distributed
    // state. We use two independent FNV-1a passes (different seeds) and
    // splice them together so the "colour" half is decoupled from the
    // "blocks" half — otherwise authors with similar emails would have
    // both similar palettes AND similar block patterns, which defeats
    // the point of a unique-looking identicon.
    // Three cheap FNV-1a passes — avoid allocating a reversed buffer
    // (the old code did `.bytes().rev().collect::<Vec<u8>>()` per paint
    // per row, which over a full-screen graph was ~100 Vec allocs per
    // frame). We walk the same buffer in reverse by index instead.
    let bytes = key.as_bytes();
    let hash_color = fnv1a(bytes, 0x811c_9dc5);
    let hash_blocks_a = fnv1a(bytes, 0xcbf2_9ce4);
    let hash_blocks_b = fnv1a_reverse(bytes, 0x01eb_5c5b);

    let fg = identicon_fg(hash_color);
    let bg = Color32::from_rgb(244, 244, 244);
    let painter = ui.painter();

    // Background plate (very light, rounded) so the identicon reads as
    // "an avatar" and not as loose confetti on the row.
    painter.rect_filled(rect, egui::Rounding::same(3.0), bg);

    let cell = AVATAR_SIZE / GRID as f32;
    // `bits` is a 64-bit stream we pull from; we need 15 bits (3 cols × 5
    // rows). Combining two 32-bit hashes gives us plenty of headroom.
    let bits: u64 = ((hash_blocks_a as u64) << 32) | (hash_blocks_b as u64);

    for row_i in 0..GRID {
        for col in 0..3 {
            let bit_idx = row_i * 3 + col;
            let filled = ((bits >> bit_idx) & 1) == 1;
            if !filled {
                continue;
            }
            // Fill left cell and its mirror on the right.
            let left = rect.left() + cell * col as f32;
            let top = rect.top() + cell * row_i as f32;
            let cell_rect = Rect::from_min_size(egui::pos2(left, top), Vec2::new(cell, cell));
            painter.rect_filled(cell_rect, egui::Rounding::ZERO, fg);

            // Mirror — skip the centre column (col 2) because mirroring
            // it would just re-paint the same cell.
            if col < 2 {
                let mirror_col = GRID - 1 - col;
                let mleft = rect.left() + cell * mirror_col as f32;
                let mirror_rect =
                    Rect::from_min_size(egui::pos2(mleft, top), Vec2::new(cell, cell));
                painter.rect_filled(mirror_rect, egui::Rounding::ZERO, fg);
            }
        }
    }
}

/// Pick a readable foreground colour for the identicon from the hash.
/// Keeps saturation moderate so the 5×5 block reads as a crisp glyph
/// rather than glowing against the row background.
fn identicon_fg(hash: u32) -> Color32 {
    // HSL: hue derived from hash, fixed saturation/lightness for
    // consistency. Lightness 45 % gives a clear contrast against the
    // near-white plate while still looking pastel-adjacent.
    let hue = (hash % 360) as f32;
    hsl_to_rgb(hue, 0.55, 0.45)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> Color32 {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c * 0.5;
    Color32::from_rgb(
        ((r1 + m) * 255.0) as u8,
        ((g1 + m) * 255.0) as u8,
        ((b1 + m) * 255.0) as u8,
    )
}

/// FNV-1a over bytes with a configurable seed. Used for identicon
/// colour + block derivation.
fn fnv1a(bytes: &[u8], seed: u32) -> u32 {
    let mut hash = seed;
    for b in bytes {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Same as `fnv1a` but walks `bytes` back-to-front — lets us derive a
/// second, independent hash from the key without allocating a reversed
/// buffer. Cost is one extra `rev()` iterator, zero heap activity.
fn fnv1a_reverse(bytes: &[u8], seed: u32) -> u32 {
    let mut hash = seed;
    for b in bytes.iter().rev() {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn paint_date_cell(ui: &mut Ui, rect: Rect, row: &GraphRow) {
    if rect.width() <= 0.0 {
        return;
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.add(
        egui::Label::new(egui::RichText::new(relative_time(row.timestamp)).weak())
            .truncate()
            .selectable(false),
    );
}

fn paint_sha_cell(ui: &mut Ui, rect: Rect, row: &GraphRow) {
    if rect.width() <= 0.0 {
        return;
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.add(
        egui::Label::new(egui::RichText::new(short_sha_str(&row.oid)).monospace())
            .truncate()
            .selectable(false),
    );
}

fn short_sha_str(oid: &gix::ObjectId) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}

#[derive(Clone, Copy, Default)]
struct WorkingTreeSummary {
    staged: usize,
    unstaged: usize,
    untracked: usize,
    conflicted: usize,
}

impl WorkingTreeSummary {
    fn has_changes(&self) -> bool {
        self.staged > 0 || self.unstaged > 0 || self.untracked > 0 || self.conflicted > 0
    }

    fn message(&self) -> String {
        let mut parts = Vec::new();
        if self.conflicted > 0 {
            parts.push(format!("{} conflicted", self.conflicted));
        }
        if self.staged > 0 {
            parts.push(format!("{} staged", self.staged));
        }
        if self.unstaged > 0 {
            parts.push(format!("{} unstaged", self.unstaged));
        }
        if self.untracked > 0 {
            parts.push(format!("{} untracked", self.untracked));
        }
        if parts.is_empty() {
            "clean".to_string()
        } else {
            parts.join(" · ")
        }
    }
}

fn summarize_working_tree(entries: &[crate::git::StatusEntry]) -> WorkingTreeSummary {
    WorkingTreeSummary {
        staged: entries.iter().filter(|e| e.staged).count(),
        unstaged: entries.iter().filter(|e| e.unstaged).count(),
        untracked: entries
            .iter()
            .filter(|e| matches!(e.kind, crate::git::EntryKind::Untracked))
            .count(),
        conflicted: entries.iter().filter(|e| e.conflicted).count(),
    }
}

fn paint_working_tree_refs_cell(ui: &mut Ui, rect: Rect, summary: &WorkingTreeSummary) {
    if rect.width() <= 0.0 {
        return;
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(rect);

    paint_ref_chip(
        &mut child,
        "WORKTREE",
        Color32::from_rgb(98, 124, 186),
        Color32::WHITE,
    );
    if summary.conflicted > 0 {
        paint_ref_chip(
            &mut child,
            "CONFLICT",
            Color32::from_rgb(192, 86, 86),
            Color32::from_rgb(255, 240, 240),
        );
    }
}

fn paint_working_tree_message_cell(ui: &mut Ui, rect: Rect, summary: &WorkingTreeSummary) {
    if rect.width() <= 0.0 {
        return;
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(rect);
    child.add(egui::Label::new(egui::RichText::new("Working tree").strong()).selectable(false));
    child.add_space(8.0);
    child.add(
        egui::Label::new(egui::RichText::new(summary.message()).weak())
            .truncate()
            .selectable(false),
    );
}

fn paint_working_tree_text_cell(ui: &mut Ui, rect: Rect, text: &str, weak: bool, monospace: bool) {
    if rect.width() <= 0.0 {
        return;
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(rect);
    let mut text = egui::RichText::new(text);
    if weak {
        text = text.weak();
    }
    if monospace {
        text = text.monospace();
    }
    child.add(egui::Label::new(text).truncate().selectable(false));
}

/// Draw a smooth S-curve from `start` to `end` as a cubic Bézier.
///
/// We choose control points that pull strongly toward the midpoint (both
/// horizontally locked to their endpoints and vertically near the middle),
/// which yields a fairly sharp transition — matching the user-requested
/// aesthetic where adjacent lanes bend in aggressively.
fn draw_lane_curve(painter: &Painter, start: Pos2, end: Pos2, stroke: Stroke) {
    // Straight segments (lane doesn't move horizontally) don't need a curve
    // and would produce a single straight Bézier anyway — skip the cost.
    if (start.x - end.x).abs() < 0.5 {
        painter.line_segment([start, end], stroke);
        return;
    }

    // Escape hatch for perf A/B: MERGEFOX_STRAIGHT_LANES=1 skips bezier
    // tessellation entirely. Each curve is replaced by a cheap 3-segment
    // polyline that still reads as "smooth" at row height. Useful on
    // slow GPUs / debug builds where the full `CubicBezierShape` path
    // can balloon into thousands of tessellated triangles per frame.
    if std::env::var("MERGEFOX_STRAIGHT_LANES").is_ok() {
        painter.line_segment([start, end], stroke);
        return;
    }

    // Vertical distance decides how "tall" the transition is; 60 % of it is
    // where our control points sit vertically, giving a noticeably sharper
    // bend than a symmetric 50 % split.
    let dy = end.y - start.y;
    let mid_y = start.y + dy * 0.6;
    let cp1 = Pos2::new(start.x, mid_y);
    let cp2 = Pos2::new(end.x, start.y + dy * 0.4);
    painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
        [start, cp1, cp2, end],
        false,
        Color32::TRANSPARENT,
        stroke,
    ));
}

fn lane_color(lane: u16) -> Color32 {
    // Pastel palette — low-saturation, high-value colours so even a merge-
    // heavy history reads as a softly coloured ribbon instead of a candy-
    // striped barcode. Hand-tuned so adjacent entries are distinguishable
    // without being garish; cycles past 12 lanes.
    const PASTEL: &[(u8, u8, u8)] = &[
        (255, 183, 178), // pink
        (255, 207, 175), // peach
        (255, 231, 175), // cream
        (206, 232, 185), // soft green
        (180, 225, 219), // seafoam
        (180, 210, 236), // sky blue
        (204, 190, 236), // lavender
        (240, 190, 220), // rose
        (195, 225, 205), // mint
        (235, 205, 185), // sand
        (215, 225, 250), // periwinkle
        (235, 230, 190), // butter
    ];
    let (r, g, b) = PASTEL[lane as usize % PASTEL.len()];
    Color32::from_rgb(r, g, b)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let h_sector = (h / 60.0) % 6.0;
    let x = c * (1.0 - (h_sector % 2.0 - 1.0).abs());
    let m = v - c;
    let (r1, g1, b1) = match h_sector as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (
        ((r1 + m) * 255.0).clamp(0.0, 255.0) as u8,
        ((g1 + m) * 255.0).clamp(0.0, 255.0) as u8,
        ((b1 + m) * 255.0).clamp(0.0, 255.0) as u8,
    )
}

fn relative_time(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let diff = now - ts;
    match diff {
        d if d < 60 => "now".to_string(),
        d if d < 3600 => format!("{}m", d / 60),
        d if d < 86_400 => format!("{}h", d / 3600),
        d if d < 2_592_000 => format!("{}d", d / 86_400),
        d => format!("{}mo", d / 2_592_000),
    }
}

/// Render Working Tree as a virtual commit row at the top of the graph.
/// This uses the same ColumnLayout as real commit rows for perfect alignment.
fn render_working_tree_row(
    ui: &mut Ui,
    layout: &ColumnLayout,
    lane_w: f32,
    graph_scroll_x: f32,
    _entries: &[crate::git::StatusEntry],
    summary: &WorkingTreeSummary,
    head_lane: u16,
    selected: &mut bool,
    out: &mut GraphInteraction,
) {
    use egui::{Sense, Vec2};

    // Allocate row space (same as commit rows)
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::click());

    let cells = layout.compute(rect);
    let is_hovered = resp.hovered();

    // Selection/hover background (same style as commits)
    if *selected {
        ui.painter().rect_filled(
            rect,
            0.0,
            ui.visuals()
                .selection
                .bg_fill
                .gamma_multiply(if ui.visuals().dark_mode { 0.42 } else { 0.18 }),
        );
    } else if is_hovered {
        ui.painter()
            .rect_filled(rect, 0.0, ui.visuals().faint_bg_color.gamma_multiply(1.1));
    }

    // Click handling
    if resp.clicked() {
        *selected = true;
        out.clear_commit_selection = true;
        out.clicked = None;
    }

    // Context menu
    resp.context_menu(|ui| {
        if ui.button("Commit…").clicked() {
            out.open_commit = true;
            ui.close_menu();
        }
        if ui.button("Create stash…").clicked() {
            out.action = Some(CommitAction::StashPushPrompt);
            ui.close_menu();
        }
    });

    // Paint virtual graph node like a normal commit row (if graph column visible)
    if layout.show_graph {
        let clipped = ui.painter().with_clip_rect(cells.graph);
        let lane_x = cells.graph.left()
            + (head_lane.min(crate::git::graph::MAX_GRAPH_LANES) as f32 + 0.5) * lane_w
            - graph_scroll_x;
        let dot_center = egui::pos2(lane_x, rect.center().y);
        let lane_color = lane_color(head_lane);
        clipped.line_segment(
            [dot_center, egui::pos2(lane_x, cells.graph.bottom())],
            egui::Stroke::new(LINE_WIDTH, lane_color),
        );
        clipped.circle_filled(dot_center, DOT_RADIUS, lane_color);
        clipped.circle_stroke(
            dot_center,
            DOT_RADIUS,
            egui::Stroke::new(1.0, Color32::from_black_alpha(80)),
        );
    }

    if layout.show_refs_column {
        paint_working_tree_refs_cell(ui, cells.refs, summary);
    }

    if layout.show_message {
        paint_working_tree_message_cell(ui, cells.message, summary);
    }

    if layout.show_author {
        paint_working_tree_text_cell(ui, cells.author, "Uncommitted", true, false);
    }
    if layout.show_date {
        paint_working_tree_text_cell(ui, cells.date, "now", true, false);
    }
    if layout.show_sha {
        paint_working_tree_text_cell(ui, cells.sha, "WT", true, true);
    }
}
