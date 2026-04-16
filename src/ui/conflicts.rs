//! Conflict resolver window.
//!
//! Visual design goals:
//!   * Surface the *operation-specific* meaning of Ours/Theirs right on the
//!     pane headers so users don't have to remember that "ours" during a
//!     rebase means the upstream tip rather than the work they're replaying.
//!   * Colour the conflict markers in the merged editor so the user can see
//!     where conflicts are without grepping for `<<<<<<<`.
//!   * Provide Prev/Next conflict navigation + a conflict counter so large
//!     files aren't a scroll-and-hunt exercise.
//!   * Offer "Take Both" alongside "Use Ours / Theirs" for the common
//!     "keep the additions from both sides" case.

use std::path::{Path, PathBuf};

use egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};
use egui::{Color32, FontId, RichText, ScrollArea, Stroke, TextEdit};

use crate::app::{MergeFoxApp, View, WorkspaceState};
use crate::git::{ConflictBlob, ConflictChoice, ConflictEntry, RepoState};

/// Palette — picked to keep ours/theirs distinguishable even on
/// dark-or-light themes. RGB values are hand-tuned; sRGB is fine for egui.
mod palette {
    use egui::Color32;
    pub const OURS: Color32 = Color32::from_rgb(86, 156, 214); // calm blue
    pub const THEIRS: Color32 = Color32::from_rgb(220, 140, 60); // warm orange
    pub const MARKER_BG_OURS: Color32 = Color32::from_rgb(30, 60, 95);
    pub const MARKER_BG_THEIRS: Color32 = Color32::from_rgb(90, 55, 25);
    pub const MARKER_BG_SPLIT: Color32 = Color32::from_rgb(60, 50, 30);
    pub const DANGER: Color32 = Color32::from_rgb(230, 80, 80);
    pub const SUCCESS: Color32 = Color32::from_rgb(80, 180, 110);
    pub const MUTED: Color32 = Color32::from_rgb(170, 170, 170);
}

/// Navigation direction for Prev/Next conflict-marker jumps.
#[derive(Clone, Copy)]
enum NavIntent {
    PrevConflict,
    NextConflict,
}

/// State for the editor's conflict-navigation cursor. Recomputed every
/// frame from the current text (cheap — conflict markers are rare).
struct ConflictMarkers {
    /// Byte offset of each `<<<<<<<` line opening a region.
    starts: Vec<usize>,
    /// Byte offset of each `>>>>>>>` line closing a region.
    ends: Vec<usize>,
}

impl ConflictMarkers {
    fn scan(text: &str) -> Self {
        let mut starts = Vec::new();
        let mut ends = Vec::new();
        let mut pos = 0;
        for line in text.split_inclusive('\n') {
            if line.starts_with("<<<<<<<") {
                starts.push(pos);
            } else if line.starts_with(">>>>>>>") {
                ends.push(pos);
            }
            pos += line.len();
        }
        Self { starts, ends }
    }

    fn count(&self) -> usize {
        self.starts.len()
    }
}

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    // --- FAST PATH ---------------------------------------------------------
    //
    // The conflict window is invisible the overwhelming majority of the time
    // (no merge / cherry-pick / rebase in progress). `conflict_entries()`
    // spawns **three** `git` subprocesses — `git diff --name-only`, then
    // `git status --porcelain`, then another `git status` via
    // `status_entries()`. On macOS each spawn is ~30–100 ms; doing that every
    // frame just to render nothing pins the main thread and makes even mouse
    // hover feel stuck.
    //
    // The two cheap, in-process signals tell us we can skip it entirely:
    //
    //   * `repo.state()` reads `.git/MERGE_HEAD` / `CHERRY_PICK_HEAD` / …
    //     marker files — pure filesystem existence checks, sub-millisecond.
    //   * `rebase_session.is_none()` is a plain field read.
    //
    // If the repo is Clean and we're not mid-replay, there cannot be
    // conflicts and we don't have to ask git.
    let (fast_clean, repo_state, rebase_summary, head_branch) = {
        let View::Workspace(tabs) = &app.view else {
            return;
        };
        let ws = tabs.current();
        let state = ws.repo.state();
        let rebase_summary = ws.rebase_session.as_ref().map(|session| {
            format!(
                "Interactive rebase on {} · step {} of {}",
                session.branch,
                session
                    .next_index
                    .saturating_add(1)
                    .min(session.steps.len()),
                session.steps.len()
            )
        });
        let clean = matches!(state, RepoState::Clean) && rebase_summary.is_none();
        (clean, state, rebase_summary, ws.repo.head_name())
    };
    if fast_clean {
        return;
    }

    // --- SLOW PATH: actually read the conflicts -----------------------------
    let conflicts = {
        let View::Workspace(tabs) = &app.view else {
            return;
        };
        let ws = tabs.current();
        match ws.repo.conflict_entries() {
            Ok(entries) => entries,
            Err(err) => {
                app.last_error = Some(format!("read conflicts: {err:#}"));
                return;
            }
        }
    };

    enum ConflictIntent {
        Use(PathBuf, ConflictChoice),
        TakeBoth(PathBuf),
        SaveManual(PathBuf, String),
        Continue,
        Abort,
    }

    let mut intent: Option<ConflictIntent> = None;
    let mut nav_intent: Option<NavIntent> = None;
    // Editor id is fixed so we can look up TextEditState across frames
    // (Prev/Next conflict navigation sets a cursor position stored by id).
    let editor_id = egui::Id::new("conflict_merged_editor");

    let labels = SideLabels::resolve(repo_state, head_branch.as_deref());

    egui::Window::new(conflict_title(repo_state, rebase_summary.as_deref()))
        .collapsible(false)
        .resizable(true)
        .default_width(1120.0)
        .default_height(780.0)
        .show(ctx, |ui| {
            let View::Workspace(tabs) = &mut app.view else {
                return;
            };
            let ws = tabs.current_mut();
            sync_selected_conflict(ws, &conflicts);

            // ---------- Header: operation summary + progress ----------
            header(ui, repo_state, rebase_summary.as_deref(), &conflicts, &labels);
            ui.separator();

            // ---------- Main content: file list + detail pane ----------
            ui.horizontal_top(|ui| {
                // File list ------------------------------------------------
                ui.vertical(|ui| {
                    ui.set_min_width(280.0);
                    ui.label(
                        RichText::new(format!("Conflicted files · {}", conflicts.len()))
                            .strong(),
                    );
                    ui.add_space(4.0);
                    ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .max_height(620.0)
                        .show(ui, |ui| {
                            for entry in &conflicts {
                                let selected =
                                    ws.selected_conflict.as_ref() == Some(&entry.path);
                                let conflict_count = entry
                                    .merged_text
                                    .as_deref()
                                    .map(|t| ConflictMarkers::scan(t).count())
                                    .unwrap_or(0);
                                if file_list_row(ui, entry, selected, conflict_count).clicked() {
                                    ws.selected_conflict = Some(entry.path.clone());
                                }
                            }
                        });
                });

                ui.separator();

                // Detail pane ----------------------------------------------
                ui.vertical(|ui| {
                    ui.set_min_width(780.0);
                    let Some(selected_path) = ws.selected_conflict.clone() else {
                        ui.weak("No conflicted files remain. Continue or abort the operation.");
                        return;
                    };
                    ensure_conflict_editor(ws, &conflicts, &selected_path);
                    let Some(entry) = conflicts.iter().find(|e| e.path == selected_path)
                    else {
                        ui.weak("Select a conflicted file to inspect it.");
                        return;
                    };

                    // File header ------------------------------------------
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format_path(&entry.path)).heading());
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if entry.is_binary {
                                    ui.colored_label(
                                        palette::DANGER,
                                        RichText::new("⛔ Binary").strong(),
                                    );
                                } else {
                                    let markers = ConflictMarkers::scan(
                                        &ws.conflict_editor_text,
                                    );
                                    let n = markers.count();
                                    let (text, color) = if n == 0 {
                                        (
                                            "✓ Ready to save".to_string(),
                                            palette::SUCCESS,
                                        )
                                    } else if n == 1 {
                                        ("⚠ 1 conflict region".to_string(), palette::DANGER)
                                    } else {
                                        (format!("⚠ {n} conflict regions"), palette::DANGER)
                                    };
                                    ui.colored_label(color, RichText::new(text).strong());
                                }
                            },
                        );
                    });

                    // Side OID summary strip
                    ui.horizontal_wrapped(|ui| {
                        side_pill(
                            ui,
                            &labels.ours_short,
                            palette::OURS,
                            entry.ours.as_ref().and_then(|b| b.oid),
                        );
                        ui.label("⇄");
                        side_pill(
                            ui,
                            &labels.theirs_short,
                            palette::THEIRS,
                            entry.theirs.as_ref().and_then(|b| b.oid),
                        );
                        if let Some(desc) = &labels.op_hint {
                            ui.weak(format!("·  {desc}"));
                        }
                    });

                    ui.add_space(6.0);

                    // Action buttons ---------------------------------------
                    ui.horizontal(|ui| {
                        let ours_btn = egui::Button::new(
                            RichText::new(format!("⬅ Use {}", labels.ours_short))
                                .color(Color32::WHITE)
                                .strong(),
                        )
                        .fill(palette::OURS);
                        ui.add_enabled_ui(entry.ours.is_some(), |ui| {
                            if ui.add(ours_btn).on_hover_text(format!(
                                "Keep the {} version, discard the {} version",
                                labels.ours_short, labels.theirs_short
                            )).clicked() {
                                intent = Some(ConflictIntent::Use(
                                    entry.path.clone(),
                                    ConflictChoice::Ours,
                                ));
                            }
                        });

                        let theirs_btn = egui::Button::new(
                            RichText::new(format!("Use {} ➡", labels.theirs_short))
                                .color(Color32::WHITE)
                                .strong(),
                        )
                        .fill(palette::THEIRS);
                        ui.add_enabled_ui(entry.theirs.is_some(), |ui| {
                            if ui.add(theirs_btn).on_hover_text(format!(
                                "Keep the {} version, discard the {} version",
                                labels.theirs_short, labels.ours_short
                            )).clicked() {
                                intent = Some(ConflictIntent::Use(
                                    entry.path.clone(),
                                    ConflictChoice::Theirs,
                                ));
                            }
                        });

                        ui.add_enabled_ui(
                            !entry.is_binary
                                && ConflictMarkers::scan(&ws.conflict_editor_text).count()
                                    > 0,
                            |ui| {
                                if ui
                                    .button("⇵ Take Both")
                                    .on_hover_text(
                                        "Keep both sides in every conflict region\n\
                                        (ours first, then theirs)",
                                    )
                                    .clicked()
                                {
                                    intent = Some(ConflictIntent::TakeBoth(entry.path.clone()));
                                }
                            },
                        );

                        ui.separator();

                        let save_btn = egui::Button::new(
                            RichText::new("💾 Save merged")
                                .color(Color32::WHITE)
                                .strong(),
                        )
                        .fill(palette::SUCCESS);
                        let has_markers =
                            ConflictMarkers::scan(&ws.conflict_editor_text).count() > 0;
                        ui.add_enabled_ui(!entry.is_binary, |ui| {
                            let btn = ui.add(save_btn);
                            let tip = if has_markers {
                                "Save the merged result as the resolution for this file.\n\
                                 Conflict markers are still present — git will flag this if they reach the working tree."
                            } else {
                                "Save the merged result as the resolution for this file."
                            };
                            if btn.on_hover_text(tip).clicked() {
                                intent = Some(ConflictIntent::SaveManual(
                                    entry.path.clone(),
                                    ws.conflict_editor_text.clone(),
                                ));
                            }
                        });
                    });

                    ui.add_space(6.0);

                    // Conflict navigation strip ----------------------------
                    if !entry.is_binary {
                        let markers = ConflictMarkers::scan(&ws.conflict_editor_text);
                        ui.horizontal(|ui| {
                            ui.weak(
                                RichText::new(format!(
                                    "{} region{}",
                                    markers.count(),
                                    if markers.count() == 1 { "" } else { "s" }
                                ))
                                .small(),
                            );
                            ui.add_enabled_ui(markers.count() > 0, |ui| {
                                if ui
                                    .small_button("⬆ Prev")
                                    .on_hover_text("Jump to previous conflict marker")
                                    .clicked()
                                {
                                    nav_intent = Some(NavIntent::PrevConflict);
                                }
                                if ui
                                    .small_button("⬇ Next")
                                    .on_hover_text("Jump to next conflict marker")
                                    .clicked()
                                {
                                    nav_intent = Some(NavIntent::NextConflict);
                                }
                            });
                        });
                    }

                    ui.add_space(4.0);

                    // Side-by-side preview ---------------------------------
                    ui.columns(2, |columns| {
                        render_blob_preview(
                            &mut columns[0],
                            &labels.ours_long,
                            palette::OURS,
                            entry.ours.as_ref(),
                        );
                        render_blob_preview(
                            &mut columns[1],
                            &labels.theirs_long,
                            palette::THEIRS,
                            entry.theirs.as_ref(),
                        );
                    });

                    ui.add_space(10.0);

                    // Merged result with conflict-marker highlighting ------
                    ui.label(
                        RichText::new("✎ Merged result (this goes to the working tree)")
                            .strong(),
                    );
                    if entry.is_binary {
                        ui.group(|ui| {
                            ui.set_min_height(180.0);
                            ui.colored_label(
                                palette::MUTED,
                                "Binary file — choose one side via the buttons above.",
                            );
                        });
                    } else {
                        let visual_dark = ui.visuals().dark_mode;
                        let mut layouter = |ui: &egui::Ui, text: &str, wrap_width: f32| {
                            let job = highlighted_layout(text, wrap_width, visual_dark);
                            ui.fonts(|f| f.layout_job(job))
                        };
                        let response = ui.add(
                            TextEdit::multiline(&mut ws.conflict_editor_text)
                                .desired_rows(18)
                                .desired_width(f32::INFINITY)
                                .id(editor_id)
                                .layouter(&mut layouter),
                        );

                        if let Some(nav) = nav_intent.take() {
                            let markers =
                                ConflictMarkers::scan(&ws.conflict_editor_text);
                            if let Some(target) =
                                pick_marker(&markers, ui.ctx(), editor_id, &nav)
                            {
                                // Move the TextEdit cursor to the chosen
                                // marker so the editor auto-scrolls to it.
                                let char_idx = ws
                                    .conflict_editor_text
                                    .get(..target)
                                    .map(|s| s.chars().count())
                                    .unwrap_or(0);
                                if let Some(mut state) =
                                    TextEdit::load_state(ui.ctx(), editor_id)
                                {
                                    state.cursor.set_char_range(Some(CCursorRange::one(
                                        CCursor::new(char_idx),
                                    )));
                                    state.store(ui.ctx(), editor_id);
                                    response.request_focus();
                                }
                            }
                        }
                    }
                });
            });

            ui.separator();

            // ---------- Footer: Abort + Continue ----------
            ui.horizontal(|ui| {
                let abort_btn = egui::Button::new(
                    RichText::new("✖ Abort")
                        .color(Color32::WHITE)
                        .strong(),
                )
                .fill(palette::DANGER);
                if ui
                    .add(abort_btn)
                    .on_hover_text(format!(
                        "Cancel the {} and restore the pre-operation state.",
                        operation_name(repo_state).to_lowercase()
                    ))
                    .clicked()
                {
                    intent = Some(ConflictIntent::Abort);
                }

                ui.weak(
                    RichText::new(
                        "  ·  tip: resolve each file, then press Continue to finish the operation.",
                    )
                    .small(),
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let can_continue = conflicts.is_empty();
                    let continue_btn = egui::Button::new(
                        RichText::new("▶ Continue")
                            .color(Color32::WHITE)
                            .strong(),
                    )
                    .fill(if can_continue {
                        palette::SUCCESS
                    } else {
                        Color32::DARK_GRAY
                    });
                    if ui
                        .add_enabled(can_continue, continue_btn)
                        .on_hover_text(if can_continue {
                            "All files resolved — finish the operation."
                        } else {
                            "Resolve every conflicted file first."
                        })
                        .clicked()
                    {
                        intent = Some(ConflictIntent::Continue);
                    }
                });
            });
        });

    match intent {
        Some(ConflictIntent::Use(path, choice)) => {
            resolve_conflict_choice(app, &path, choice);
        }
        Some(ConflictIntent::TakeBoth(path)) => {
            let merged = if let View::Workspace(tabs) = &app.view {
                take_both(&tabs.current().conflict_editor_text)
            } else {
                return;
            };
            // Replace the editor contents so the user can still tweak
            // before saving, rather than saving immediately.
            if let View::Workspace(tabs) = &mut app.view {
                tabs.current_mut().conflict_editor_text = merged.clone();
            }
            app.hud = Some(crate::app::Hud::new(
                format!("Combined both sides in {}", format_path(&path)),
                1800,
            ));
        }
        Some(ConflictIntent::SaveManual(path, text)) => {
            resolve_conflict_manual(app, &path, &text);
        }
        Some(ConflictIntent::Continue) => app.continue_conflict_operation(),
        Some(ConflictIntent::Abort) => app.abort_conflict_operation(),
        None => {}
    }
}

/// File-list row with an inline conflict-count badge.
fn file_list_row(
    ui: &mut egui::Ui,
    entry: &ConflictEntry,
    selected: bool,
    conflict_count: usize,
) -> egui::Response {
    let label_text = format_path(&entry.path);
    let full_frame = egui::Frame::default().inner_margin(egui::Margin::symmetric(4.0, 2.0));
    full_frame
        .show(ui, |ui| {
            // Use a horizontal layout so badges sit on the right. The entire
            // row is a single `selectable_label` wrapping the horizontal
            // strip, so click targets match the visible row.
            let resp = ui
                .horizontal(|ui| {
                    ui.set_width(ui.available_width());
                    let path_resp = ui.selectable_label(selected, label_text);
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if entry.is_binary {
                                ui.colored_label(palette::DANGER, "Bin");
                            } else if conflict_count > 0 {
                                ui.colored_label(
                                    palette::DANGER,
                                    RichText::new(format!("⚠{conflict_count}")).small().strong(),
                                );
                            }
                        },
                    );
                    path_resp
                })
                .inner;
            resp
        })
        .inner
}

fn header(
    ui: &mut egui::Ui,
    state: RepoState,
    rebase_summary: Option<&str>,
    conflicts: &[ConflictEntry],
    labels: &SideLabels,
) {
    ui.horizontal(|ui| {
        let title = rebase_summary
            .map(str::to_string)
            .unwrap_or_else(|| operation_summary(state, conflicts.len()));
        ui.label(RichText::new(title).strong());
    });
    ui.horizontal_wrapped(|ui| {
        ui.weak("Ours");
        ui.colored_label(palette::OURS, RichText::new(&labels.ours_long).strong());
        ui.weak("·  Theirs");
        ui.colored_label(palette::THEIRS, RichText::new(&labels.theirs_long).strong());
    });
    ui.add_space(2.0);
    ui.weak(
        "Pick a side, combine both, or edit the merged result below. Save each file, \
         then press Continue to finish the operation.",
    );
}

fn side_pill(ui: &mut egui::Ui, label: &str, color: Color32, oid: Option<gix::ObjectId>) {
    egui::Frame::none()
        .fill(color.gamma_multiply(0.25))
        .stroke(Stroke::new(1.0, color))
        .inner_margin(egui::Margin::symmetric(6.0, 2.0))
        .rounding(egui::Rounding::same(4.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(label)
                        .monospace()
                        .color(color)
                        .small()
                        .strong(),
                );
                if let Some(oid) = oid {
                    ui.weak(RichText::new(short_sha(&oid)).monospace().small());
                }
            });
        });
}

/// Context-specific "Ours / Theirs" labels so the user doesn't have to
/// remember which is which per-operation.
struct SideLabels {
    /// Tight label, e.g. "Ours" / "HEAD" / "Incoming".
    ours_short: String,
    theirs_short: String,
    /// Long label with extra context, e.g. "Ours — feature-x (HEAD)".
    ours_long: String,
    theirs_long: String,
    /// Optional one-line hint about the operation semantics.
    op_hint: Option<String>,
}

impl SideLabels {
    fn resolve(state: RepoState, head_branch: Option<&str>) -> Self {
        let branch_suffix = head_branch
            .map(|b| format!(" ({b})"))
            .unwrap_or_default();
        match state {
            RepoState::Merge => Self {
                ours_short: "Ours".into(),
                theirs_short: "Theirs".into(),
                ours_long: format!("Ours — current branch{branch_suffix}"),
                theirs_long: "Theirs — being merged in".into(),
                op_hint: Some("merge: keeping ours means keeping your branch".into()),
            },
            RepoState::CherryPick | RepoState::CherryPickSequence => Self {
                ours_short: "Ours".into(),
                theirs_short: "Theirs".into(),
                ours_long: format!("Ours — current tree{branch_suffix}"),
                theirs_long: "Theirs — the picked commit".into(),
                op_hint: Some("cherry-pick: theirs = the change you're applying".into()),
            },
            RepoState::Revert | RepoState::RevertSequence => Self {
                ours_short: "Ours".into(),
                theirs_short: "Theirs".into(),
                ours_long: format!("Ours — current tree{branch_suffix}"),
                theirs_long: "Theirs — undo of the reverted commit".into(),
                op_hint: Some("revert: theirs = the removal you're applying".into()),
            },
            RepoState::Rebase
            | RepoState::RebaseInteractive
            | RepoState::RebaseMerge
            | RepoState::ApplyMailbox
            | RepoState::ApplyMailboxOrRebase => Self {
                ours_short: "Base".into(),
                theirs_short: "Your change".into(),
                ours_long: "Ours — target branch + already-applied commits".into(),
                theirs_long: "Theirs — YOUR commit being replayed".into(),
                op_hint: Some(
                    "rebase reverses the usual sides: \"theirs\" is your own work".into(),
                ),
            },
            _ => Self {
                ours_short: "Ours".into(),
                theirs_short: "Theirs".into(),
                ours_long: format!("Ours — current tree{branch_suffix}"),
                theirs_long: "Theirs — incoming change".into(),
                op_hint: None,
            },
        }
    }
}

fn sync_selected_conflict(ws: &mut WorkspaceState, conflicts: &[ConflictEntry]) {
    let selected_valid = ws
        .selected_conflict
        .as_ref()
        .is_some_and(|path| conflicts.iter().any(|entry| &entry.path == path));
    if !selected_valid {
        ws.selected_conflict = conflicts.first().map(|entry| entry.path.clone());
    }

    if let Some(path) = ws.selected_conflict.clone() {
        ensure_conflict_editor(ws, conflicts, &path);
    } else {
        ws.conflict_editor_path = None;
        ws.conflict_editor_text.clear();
    }
}

fn ensure_conflict_editor(ws: &mut WorkspaceState, conflicts: &[ConflictEntry], path: &Path) {
    if ws.conflict_editor_path.as_deref() == Some(path) {
        return;
    }

    let text = conflicts
        .iter()
        .find(|entry| entry.path == path)
        .and_then(|entry| {
            entry
                .merged_text
                .clone()
                .or_else(|| entry.ours.as_ref().and_then(|blob| blob.text.clone()))
                .or_else(|| entry.theirs.as_ref().and_then(|blob| blob.text.clone()))
        })
        .unwrap_or_default();
    ws.conflict_editor_path = Some(path.to_path_buf());
    ws.conflict_editor_text = text;
}

fn resolve_conflict_choice(app: &mut MergeFoxApp, path: &Path, choice: ConflictChoice) {
    let result = {
        let View::Workspace(tabs) = &mut app.view else {
            return;
        };
        let ws = tabs.current_mut();
        let result = ws.repo.resolve_conflict_choice(path, choice);
        ws.conflict_editor_path = None;
        result
    };

    match result {
        Ok(()) => {
            app.hud = Some(crate::app::Hud::new(
                format!(
                    "Resolved {} with {}",
                    format_path(path),
                    choice_label(choice)
                ),
                1800,
            ));
        }
        Err(err) => {
            app.last_error = Some(format!("resolve conflict: {err:#}"));
        }
    }
}

fn resolve_conflict_manual(app: &mut MergeFoxApp, path: &Path, text: &str) {
    let result = {
        let View::Workspace(tabs) = &mut app.view else {
            return;
        };
        let ws = tabs.current_mut();
        let result = ws.repo.resolve_conflict_manual(path, text);
        ws.conflict_editor_path = None;
        result
    };

    match result {
        Ok(()) => {
            app.hud = Some(crate::app::Hud::new(
                format!("Saved manual resolution for {}", format_path(path)),
                1800,
            ));
        }
        Err(err) => {
            app.last_error = Some(format!("manual resolution: {err:#}"));
        }
    }
}

fn render_blob_preview(
    ui: &mut egui::Ui,
    title: &str,
    accent: Color32,
    blob: Option<&ConflictBlob>,
) {
    egui::Frame::group(ui.style())
        .stroke(Stroke::new(1.5, accent))
        .show(ui, |ui| {
            ui.set_min_width(360.0);
            ui.horizontal(|ui| {
                ui.colored_label(accent, RichText::new(title).strong());
                if let Some(blob) = blob {
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.weak(format_size(blob.size));
                            if let Some(oid) = blob.oid {
                                ui.weak(RichText::new(short_sha(&oid)).monospace());
                            }
                        },
                    );
                }
            });
            match blob {
                Some(blob) => {
                    ui.separator();
                    if let Some(text) = &blob.text {
                        let mut preview = text.clone();
                        ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .max_height(240.0)
                            .show(ui, |ui| {
                                ui.add(
                                    TextEdit::multiline(&mut preview)
                                        .desired_rows(12)
                                        .desired_width(f32::INFINITY)
                                        .font(egui::TextStyle::Monospace)
                                        .interactive(false),
                                );
                            });
                    } else {
                        ui.weak("Binary content");
                    }
                }
                None => {
                    ui.weak("No version on this side (file added/deleted).");
                }
            }
        });
}

/// Turn raw text with `<<<<<<<` / `=======` / `>>>>>>>` markers into a
/// coloured `LayoutJob` so the editor shows conflict regions at a glance.
fn highlighted_layout(text: &str, wrap_width: f32, _dark: bool) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    let font = FontId::monospace(13.0);

    #[derive(Clone, Copy, PartialEq)]
    enum Region {
        Normal,
        Ours,
        Theirs,
    }
    let mut region = Region::Normal;

    // We walk line-by-line; `split_inclusive` keeps the trailing newline
    // attached so byte offsets stay exact and the output text reads back
    // identical to the input.
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);

        if trimmed.starts_with("<<<<<<<") {
            region = Region::Ours;
            job.append(
                line,
                0.0,
                TextFormat {
                    font_id: font.clone(),
                    color: palette::OURS,
                    background: palette::MARKER_BG_OURS,
                    ..TextFormat::default()
                },
            );
            continue;
        }
        if trimmed.starts_with("=======") && region != Region::Normal {
            region = Region::Theirs;
            job.append(
                line,
                0.0,
                TextFormat {
                    font_id: font.clone(),
                    color: Color32::WHITE,
                    background: palette::MARKER_BG_SPLIT,
                    ..TextFormat::default()
                },
            );
            continue;
        }
        if trimmed.starts_with(">>>>>>>") && region != Region::Normal {
            job.append(
                line,
                0.0,
                TextFormat {
                    font_id: font.clone(),
                    color: palette::THEIRS,
                    background: palette::MARKER_BG_THEIRS,
                    ..TextFormat::default()
                },
            );
            region = Region::Normal;
            continue;
        }

        // Non-marker lines: tint background for "ours" / "theirs" regions
        // so the coloured block stretches over the whole region.
        let format = match region {
            Region::Ours => TextFormat {
                font_id: font.clone(),
                background: palette::MARKER_BG_OURS.linear_multiply(0.45),
                color: Color32::LIGHT_GRAY,
                ..TextFormat::default()
            },
            Region::Theirs => TextFormat {
                font_id: font.clone(),
                background: palette::MARKER_BG_THEIRS.linear_multiply(0.45),
                color: Color32::LIGHT_GRAY,
                ..TextFormat::default()
            },
            Region::Normal => TextFormat {
                font_id: font.clone(),
                color: Color32::LIGHT_GRAY,
                ..TextFormat::default()
            },
        };
        job.append(line, 0.0, format);
    }

    job
}

/// Pick the next / previous conflict-marker offset relative to the current
/// cursor position (taken from `TextEditState`). Returns the byte offset
/// in `text` to jump to, or `None` if there's no marker to move to.
fn pick_marker(
    markers: &ConflictMarkers,
    ctx: &egui::Context,
    editor_id: egui::Id,
    nav: &NavIntent,
) -> Option<usize> {
    if markers.starts.is_empty() {
        return None;
    }
    let current_char = TextEdit::load_state(ctx, editor_id)
        .and_then(|state| {
            state
                .cursor
                .char_range()
                .map(|r| r.primary.index)
        })
        .unwrap_or(0);

    // We compare in BYTE offsets because that's how we recorded markers.
    // Approximation: character index == byte index is only correct for
    // ASCII, but source files with conflicts are overwhelmingly ASCII in
    // the marker vicinity, and the jump is coarse anyway.
    let current_byte = current_char;
    match nav {
        NavIntent::NextConflict => markers
            .starts
            .iter()
            .copied()
            .find(|&m| m > current_byte)
            .or_else(|| markers.starts.first().copied()),
        NavIntent::PrevConflict => markers
            .starts
            .iter()
            .rev()
            .copied()
            .find(|&m| m < current_byte)
            .or_else(|| markers.starts.last().copied()),
    }
}

/// Combine both sides (ours first, then theirs) in every conflict region.
fn take_both(text: &str) -> String {
    let mut out = String::new();
    let mut in_conflict = false;
    let mut in_theirs = false;
    let mut ours = String::new();
    let mut theirs = String::new();
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.starts_with("<<<<<<<") {
            in_conflict = true;
            in_theirs = false;
            ours.clear();
            theirs.clear();
        } else if trimmed.starts_with("=======") && in_conflict {
            in_theirs = true;
        } else if trimmed.starts_with(">>>>>>>") && in_conflict {
            out.push_str(&ours);
            out.push_str(&theirs);
            in_conflict = false;
            in_theirs = false;
        } else if in_conflict {
            if in_theirs {
                theirs.push_str(line);
            } else {
                ours.push_str(line);
            }
        } else {
            out.push_str(line);
        }
    }
    out
}

fn conflict_title(state: RepoState, rebase_summary: Option<&str>) -> String {
    if rebase_summary.is_some() {
        "Conflict Resolver · Rebase".to_string()
    } else {
        format!("Conflict Resolver · {}", operation_name(state))
    }
}

fn operation_summary(state: RepoState, conflict_count: usize) -> String {
    if conflict_count == 0 {
        format!(
            "{} is ready to continue. Review the result, then continue or abort.",
            operation_name(state)
        )
    } else {
        format!(
            "{} · {} conflicted file{}",
            operation_name(state),
            conflict_count,
            if conflict_count == 1 { "" } else { "s" }
        )
    }
}

fn operation_name(state: RepoState) -> &'static str {
    match state {
        RepoState::Merge => "Merge",
        RepoState::CherryPick | RepoState::CherryPickSequence => "Cherry-pick",
        RepoState::Revert | RepoState::RevertSequence => "Revert",
        RepoState::Rebase
        | RepoState::RebaseInteractive
        | RepoState::RebaseMerge
        | RepoState::ApplyMailbox
        | RepoState::ApplyMailboxOrRebase => "Rebase",
        _ => "Git operation",
    }
}

fn short_sha(oid: &gix::ObjectId) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}

fn format_path(path: &Path) -> String {
    path.display().to_string()
}

fn format_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = KB * 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{bytes} B")
    }
}

fn choice_label(choice: ConflictChoice) -> &'static str {
    match choice {
        ConflictChoice::Ours => "ours",
        ConflictChoice::Theirs => "theirs",
    }
}
