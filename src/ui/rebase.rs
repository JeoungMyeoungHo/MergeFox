//! Interactive-rebase planning modal.
//!
//! Visual model (Tower-inspired, see user screenshot):
//!   ┌─ Rebase: <branch>              On:  <base sha> <base subject>──┐
//!   │ ● Pick ▾ ↕  subject              …  author   sha   date        │
//!   │ ● Reword ▾ ↕  subject            …  author   sha   date        │
//!   │ ◯ Squash ▾ ↕  [bracket] subject  …  author   sha   date        │
//!   │ ● Drop ▾ ↕    [strikethrough]    …  [grey]   [-]   [-]         │
//!   │                                                                │
//!   │ ── Commit | Changes ─────────────────────────────────────────── │
//!   │   AUTHOR:   <name> <email>   COMMITTER:  <name> <email>        │
//!   │   SHA:      <full>           PARENTS:    <short>               │
//!   │   <subject / body>                                             │
//!   │                                                                │
//!   │ [✓] Backup current state with tag         Cancel  [ Rebase ]   │
//!   └────────────────────────────────────────────────────────────────┘
//!
//! Key behaviours:
//!   * Action dot colour communicates the action at a glance (green Pick,
//!     yellow Reword, grey Squash, red Drop).
//!   * Dropped rows render with a strikethrough + muted palette.
//!   * Squash rows draw a bracket line up to the previous non-squash row
//!     so the user can see which commit the squashed one merges into.
//!   * The detail pane at the bottom has `Commit` / `Changes` tabs; the
//!     Changes tab shows the selected commit's diff via `diff_for_commit`.

use std::time::{SystemTime, UNIX_EPOCH};

use egui::{Color32, RichText, Rounding, ScrollArea, Stroke, TextEdit};

use crate::app::{MergeFoxApp, RebaseAction, View};
use crate::git::{DeltaStatus, RepoDiff};

/// Which bottom-pane tab is active. Re-used across frames via
/// `egui::Memory` so we don't need to plumb state through the modal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DetailTab {
    Commit,
    Changes,
}
impl Default for DetailTab {
    fn default() -> Self {
        Self::Commit
    }
}

mod palette {
    use egui::Color32;
    /// Green dot / text tint for Pick.
    pub const PICK: Color32 = Color32::from_rgb(112, 184, 120);
    /// Warm yellow for Reword.
    pub const REWORD: Color32 = Color32::from_rgb(222, 180, 80);
    /// Muted grey for Squash.
    pub const SQUASH: Color32 = Color32::from_rgb(160, 160, 170);
    /// Desaturated muted grey-blue for Fixup — same shape as Squash
    /// but cooler so the two are telegraphed apart at a glance.
    pub const FIXUP: Color32 = Color32::from_rgb(130, 150, 180);
    /// Red for Drop.
    pub const DROP: Color32 = Color32::from_rgb(218, 90, 90);
    /// Soft accent used around the bottom detail pane.
    pub const ACCENT: Color32 = Color32::from_rgb(120, 160, 220);
    /// Legible grey for struck-through text.
    pub const MUTED: Color32 = Color32::from_rgb(150, 150, 150);
}

fn action_color(action: RebaseAction) -> Color32 {
    match action {
        RebaseAction::Pick => palette::PICK,
        RebaseAction::Reword => palette::REWORD,
        RebaseAction::Squash => palette::SQUASH,
        RebaseAction::Fixup => palette::FIXUP,
        RebaseAction::Drop => palette::DROP,
    }
}

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    let has_modal = matches!(
        &app.view,
        View::Workspace(tabs) if tabs.current().rebase_modal.is_some()
    );
    if !has_modal {
        return;
    }

    let detail_tab_id = egui::Id::new("rebase_detail_tab");
    let mut detail_tab: DetailTab = ctx
        .data(|d| d.get_temp::<DetailTab>(detail_tab_id))
        .unwrap_or_default();

    let mut open = true;
    let mut cancel = false;
    let mut start = false;

    egui::Window::new("Interactive Rebase")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(1120.0)
        .default_height(760.0)
        .show(ctx, |ui| {
            let View::Workspace(tabs) = &mut app.view else {
                return;
            };
            let repo_path = tabs.current().repo.path().to_path_buf();
            let ws = tabs.current_mut();
            let Some(modal) = ws.rebase_modal.as_mut() else {
                return;
            };

            let mut move_up: Option<usize> = None;
            let mut move_down: Option<usize> = None;
            let mut select_idx: Option<usize> = None;

            // ---------- Header bar: "Rebase: <branch>   On:  <sha>" -----
            ui.horizontal(|ui| {
                ui.label(RichText::new("Rebase:").strong());
                ui.label(
                    RichText::new(format!("🜲 {}", modal.branch))
                        .color(palette::ACCENT)
                        .strong(),
                );
                ui.add_space(24.0);
                ui.label(RichText::new("On:").strong());
                ui.label(
                    RichText::new(format!("◆ {}", short_sha(&modal.base)))
                        .monospace()
                        .color(palette::ACCENT),
                );
            });

            ui.add_space(4.0);
            ui.weak("Reorder commits with ↑/↓, pick an action per commit, then press Rebase.");

            if let Some(err) = &modal.last_error {
                ui.add_space(4.0);
                ui.colored_label(Color32::LIGHT_RED, err);
            }

            ui.separator();

            // ---------- Plan list -----------------------------------------
            let item_count = modal.items.len();
            // Precompute each item's effective target-commit-above for
            // Squash bracket rendering: for every item, which index
            // (if any) does its squashing merge INTO?
            let squash_targets = compute_squash_targets(&modal.items);

            ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height(360.0)
                .show(ui, |ui| {
                    for idx in 0..item_count {
                        let can_move_up = idx > 0;
                        let can_move_down = idx + 1 < item_count;
                        let selected = modal.selected_idx == idx;
                        let squash_target = squash_targets[idx];

                        let item = &mut modal.items[idx];
                        let dimmed = matches!(item.action, RebaseAction::Drop);
                        let squashed = item.action.rolls_into_parent();

                        render_plan_row(
                            ui,
                            idx,
                            item,
                            selected,
                            dimmed,
                            squashed,
                            squash_target,
                            can_move_up,
                            can_move_down,
                            &mut select_idx,
                            &mut move_up,
                            &mut move_down,
                        );
                    }
                });

            ui.add_space(4.0);
            ui.separator();

            // ---------- Detail pane: Commit / Changes tabs ---------------
            ui.horizontal(|ui| {
                let tab_commit = ui.selectable_label(
                    detail_tab == DetailTab::Commit,
                    RichText::new("Commit").strong(),
                );
                if tab_commit.clicked() {
                    detail_tab = DetailTab::Commit;
                }
                let tab_changes = ui.selectable_label(
                    detail_tab == DetailTab::Changes,
                    RichText::new("Changes").strong(),
                );
                if tab_changes.clicked() {
                    detail_tab = DetailTab::Changes;
                }
            });
            ui.add_space(2.0);

            if let Some(item) = modal.items.get_mut(modal.selected_idx) {
                match detail_tab {
                    DetailTab::Commit => render_commit_detail(ui, item),
                    DetailTab::Changes => render_changes_detail(ui, &repo_path, item.oid),
                }
            } else {
                ui.weak("No commit selected.");
            }

            // Apply row moves after render (swap index & slide selection).
            if let Some(idx) = select_idx {
                modal.selected_idx = idx;
            }
            if let Some(idx) = move_up {
                modal.items.swap(idx, idx - 1);
                modal.selected_idx = idx - 1;
            }
            if let Some(idx) = move_down {
                modal.items.swap(idx, idx + 1);
                modal.selected_idx = idx + 1;
            }

            ui.separator();

            // ---------- Footer: backup checkbox + Cancel / Rebase --------
            ui.horizontal(|ui| {
                ui.checkbox(
                    &mut modal.backup_current_state,
                    "Backup current state with tag",
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let rebase_btn =
                        egui::Button::new(RichText::new("Rebase").color(Color32::WHITE).strong())
                            .fill(palette::ACCENT)
                            .min_size(egui::vec2(96.0, 26.0));
                    if ui.add(rebase_btn).clicked() {
                        start = true;
                    }
                    if ui.button(RichText::new("Cancel")).clicked() {
                        cancel = true;
                    }
                });
            });
        });

    ctx.data_mut(|d| d.insert_temp(detail_tab_id, detail_tab));

    if !open || cancel {
        if let View::Workspace(tabs) = &mut app.view {
            tabs.current_mut().rebase_modal = None;
        }
    }
    if start {
        app.start_rebase_session();
    }
}

/// For each plan item, if it's a Squash, return the index of the
/// immediately-prior non-squash item — that's the commit it'll be merged
/// into. Used to draw the bracket connector so the user can visualise the
/// squash target without guessing.
fn compute_squash_targets(items: &[crate::app::RebasePlanItem]) -> Vec<Option<usize>> {
    let mut out = vec![None; items.len()];
    for (i, item) in items.iter().enumerate() {
        if item.action.rolls_into_parent() {
            // Walk backwards to find the nearest non-Squash, non-Fixup,
            // non-Drop anchor. That's the commit that keeps the
            // history entry; our squash/fixup rolls into it.
            for j in (0..i).rev() {
                if items[j].action.rolls_into_parent()
                    || matches!(items[j].action, RebaseAction::Drop)
                {
                    continue;
                }
                out[i] = Some(j);
                break;
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn render_plan_row(
    ui: &mut egui::Ui,
    idx: usize,
    item: &mut crate::app::RebasePlanItem,
    selected: bool,
    dimmed: bool,
    squashed: bool,
    squash_target: Option<usize>,
    can_move_up: bool,
    can_move_down: bool,
    select_idx: &mut Option<usize>,
    move_up: &mut Option<usize>,
    move_down: &mut Option<usize>,
) {
    let accent = action_color(item.action);
    let row_bg = if selected {
        Color32::from_rgb(60, 110, 190)
    } else {
        Color32::TRANSPARENT
    };
    let text_color = if dimmed {
        palette::MUTED
    } else if selected {
        Color32::WHITE
    } else {
        ui.visuals().text_color()
    };

    let strike = if dimmed {
        egui::Stroke::new(1.0, palette::MUTED)
    } else {
        egui::Stroke::NONE
    };

    egui::Frame::none()
        .fill(row_bg)
        .inner_margin(egui::Margin::symmetric(4.0, 2.0))
        .rounding(Rounding::same(2.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // Action dot (clickable selection)
                let (dot_rect, dot_resp) =
                    ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::click());
                ui.painter().circle_filled(dot_rect.center(), 5.0, accent);
                if dimmed {
                    // Hollow out for Drop so it reads as "removed"
                    ui.painter()
                        .circle_stroke(dot_rect.center(), 5.0, Stroke::new(1.5, accent));
                }
                if dot_resp.clicked() {
                    *select_idx = Some(idx);
                }

                // Action label + inline dropdown.
                egui::ComboBox::from_id_salt(("rebase_action", idx))
                    .selected_text(RichText::new(item.action.label()).color(accent).strong())
                    .width(82.0)
                    .show_ui(ui, |ui| {
                        for action in [
                            RebaseAction::Pick,
                            RebaseAction::Reword,
                            RebaseAction::Squash,
                            RebaseAction::Fixup,
                            RebaseAction::Drop,
                        ] {
                            let label = RichText::new(action.label()).color(action_color(action));
                            let tip = match action {
                                RebaseAction::Pick => "Keep this commit as-is.",
                                RebaseAction::Reword => "Keep the commit, edit its message.",
                                RebaseAction::Squash =>
                                    "Merge into the previous kept commit; combine both messages in an editor.",
                                RebaseAction::Fixup =>
                                    "Merge into the previous kept commit; discard this commit's message.",
                                RebaseAction::Drop => "Remove this commit from history.",
                            };
                            ui.selectable_value(&mut item.action, action, label)
                                .on_hover_text(tip);
                        }
                    });

                // Reorder arrows ↑ / ↓
                ui.vertical(|ui| {
                    ui.spacing_mut().button_padding = egui::vec2(2.0, 0.0);
                    ui.add_enabled_ui(can_move_up, |ui| {
                        if ui.small_button("▲").clicked() {
                            *move_up = Some(idx);
                        }
                    });
                    ui.add_enabled_ui(can_move_down, |ui| {
                        if ui.small_button("▼").clicked() {
                            *move_down = Some(idx);
                        }
                    });
                });

                ui.add_space(4.0);

                // Squash bracket: a small glyph that indicates this row
                // squashes into another. We don't draw a real bracket path
                // (would require custom painting within a horizontal strip)
                // — the indent + glyph + tooltip communicates the same.
                if squashed && squash_target.is_some() {
                    ui.label(
                        RichText::new("↳")
                            .color(palette::SQUASH)
                            .monospace()
                            .strong(),
                    )
                    .on_hover_text(format!(
                        "Squashes into commit #{}",
                        squash_target.map(|i| i + 1).unwrap_or(0)
                    ));
                } else if squashed {
                    ui.label(RichText::new("↳").color(palette::DROP).monospace().strong())
                        .on_hover_text("Squash has no preceding commit to merge into!");
                }

                // Subject (selectable). We use selectable_label + ui.interact
                // to make the whole subject cell clickable for selection.
                let mut subject = RichText::new(item.summary.as_str()).color(text_color);
                if dimmed {
                    subject = subject.strikethrough();
                } else if squashed {
                    subject = subject.color(palette::SQUASH);
                }
                let subj_resp = ui.add(
                    egui::Label::new(subject)
                        .truncate()
                        .sense(egui::Sense::click()),
                );
                if subj_resp.clicked() {
                    *select_idx = Some(idx);
                }

                // Right-aligned metadata: author · sha · date.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let date = relative_time(item.timestamp);
                    let sha = short_sha(&item.oid);
                    let meta = format!("{}   {}   {}", item.author, sha, date);
                    let mut rt = RichText::new(meta)
                        .color(if dimmed {
                            palette::MUTED
                        } else if selected {
                            Color32::from_rgba_unmultiplied(255, 255, 255, 210)
                        } else {
                            palette::MUTED
                        })
                        .monospace()
                        .small();
                    if dimmed {
                        rt = rt.strikethrough();
                    }
                    ui.add(egui::Label::new(rt).truncate());
                });
                // Keep strike stroke variable used (silence lints when
                // the theme doesn't surface it visually).
                let _ = strike;
            });
        });
}

fn render_commit_detail(ui: &mut egui::Ui, item: &mut crate::app::RebasePlanItem) {
    ScrollArea::vertical()
        .auto_shrink([false, true])
        .max_height(240.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.weak("AUTHOR");
                    ui.label(RichText::new(&item.author).strong());
                });
                ui.add_space(28.0);
                ui.vertical(|ui| {
                    ui.weak("SHA");
                    ui.label(RichText::new(item.oid.to_string()).monospace().small());
                });
                ui.add_space(28.0);
                ui.vertical(|ui| {
                    ui.weak("WHEN");
                    ui.label(relative_time(item.timestamp));
                });
                ui.add_space(28.0);
                ui.vertical(|ui| {
                    ui.weak("ACTION");
                    ui.colored_label(
                        action_color(item.action),
                        RichText::new(item.action.label()).strong(),
                    );
                });
            });

            ui.separator();

            match item.action {
                RebaseAction::Reword | RebaseAction::Squash => {
                    let label = if matches!(item.action, RebaseAction::Squash) {
                        "Squash message (will replace the target commit's message)"
                    } else {
                        "Reworded message"
                    };
                    ui.label(RichText::new(label).strong());
                    ui.add(
                        TextEdit::multiline(&mut item.edited_message)
                            .desired_rows(8)
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Monospace),
                    );
                }
                RebaseAction::Fixup => {
                    ui.colored_label(
                        palette::FIXUP,
                        "This commit will be merged into the previous kept commit; its message will be discarded.",
                    );
                    let mut preview = item.original_message.clone();
                    ui.add(
                        TextEdit::multiline(&mut preview)
                            .desired_rows(6)
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Monospace)
                            .interactive(false),
                    );
                    ui.weak("Switch to Squash if you want to keep / edit this commit's message.");
                }
                RebaseAction::Pick => {
                    ui.label(RichText::new("Commit message").strong());
                    let mut preview = item.original_message.clone();
                    ui.add(
                        TextEdit::multiline(&mut preview)
                            .desired_rows(8)
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Monospace)
                            .interactive(false),
                    );
                    ui.weak("Switch to Reword to edit this message.");
                }
                RebaseAction::Drop => {
                    ui.colored_label(
                        palette::DROP,
                        "This commit will be dropped — its changes will NOT be applied.",
                    );
                    let mut preview = item.original_message.clone();
                    ui.add(
                        TextEdit::multiline(&mut preview)
                            .desired_rows(8)
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Monospace)
                            .interactive(false),
                    );
                }
            }
        });
}

/// "Changes" tab — shows a compact file list for the selected commit's
/// diff. Diff computation runs synchronously inside the modal (it's only
/// for the one selected commit, and this modal is already blocking).
fn render_changes_detail(ui: &mut egui::Ui, repo_path: &std::path::Path, oid: gix::ObjectId) {
    // Cache the diff by oid in memory so re-selecting the same row
    // doesn't re-invoke git.
    let cache_id = egui::Id::new(("rebase_changes_diff", oid));
    let cached: Option<RepoDiff> = ui.ctx().data(|d| d.get_temp::<RepoDiff>(cache_id));
    let diff: Result<RepoDiff, String> = match cached {
        Some(d) => Ok(d),
        None => match crate::git::diff_for_commit(repo_path, oid) {
            Ok(d) => {
                ui.ctx()
                    .data_mut(|mem| mem.insert_temp(cache_id, d.clone()));
                Ok(d)
            }
            Err(e) => Err(format!("{e:#}")),
        },
    };

    ScrollArea::vertical()
        .auto_shrink([false, true])
        .max_height(240.0)
        .show(ui, |ui| match diff {
            Ok(diff) if diff.files.is_empty() => {
                ui.weak("No file changes (empty commit).");
            }
            Ok(diff) => {
                ui.weak(format!("{} file(s) changed", diff.files.len()));
                ui.add_space(2.0);
                for file in diff.files.iter() {
                    let (color, glyph) = status_glyph(file.status);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(glyph).color(color).monospace().strong());
                        ui.label(file.display_path());
                    });
                }
            }
            Err(err) => {
                ui.colored_label(Color32::LIGHT_RED, format!("diff: {err}"));
            }
        });
}

fn status_glyph(status: DeltaStatus) -> (Color32, &'static str) {
    match status {
        DeltaStatus::Added => (Color32::from_rgb(120, 200, 140), "A"),
        DeltaStatus::Modified => (Color32::from_rgb(220, 190, 90), "M"),
        DeltaStatus::Deleted => (Color32::from_rgb(220, 100, 100), "D"),
        DeltaStatus::Renamed => (Color32::from_rgb(160, 170, 220), "R"),
        DeltaStatus::Copied => (Color32::from_rgb(160, 170, 220), "C"),
        DeltaStatus::Typechange => (Color32::from_rgb(200, 120, 200), "T"),
        DeltaStatus::Unmodified => (palette::MUTED, "·"),
    }
}

fn short_sha(oid: &gix::ObjectId) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}

fn relative_time(ts: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let diff = now - ts;
    match diff {
        d if d < 60 => "now".to_string(),
        d if d < 3600 => format!("{}m ago", d / 60),
        d if d < 86_400 => format!("{}h ago", d / 3600),
        d if d < 2_592_000 => format!("{}d ago", d / 86_400),
        d => format!("{}mo ago", d / 2_592_000),
    }
}
