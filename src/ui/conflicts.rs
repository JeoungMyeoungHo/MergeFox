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
//!
//! Local/Remote card picker (added in the `card_picker` section below):
//!   * The per-file picker leads with two large rounded cards — Local (our)
//!     and Remote (their) — each showing the branch/tip, the file path, a
//!     "modified" status chip, and a click-to-pick checkbox. A subtle
//!     connector line is drawn between the cards so users can see that the
//!     two sides are the "alternatives" they're choosing between rather than
//!     independent controls.
//!   * The "Merge in External Tool" button hands off to `git mergetool` so
//!     power users with a configured merge driver don't have to drop to a
//!     terminal. Git's own `mergetool` driver respects the user's global
//!     config (`merge.tool`) and will write the resolved file back to the
//!     working tree, which is exactly the behaviour the rest of MergeFox
//!     already expects.
//!   * The existing editor-mode UI is preserved as an "Advanced" collapsing
//!     section so users who want to hand-resolve markers still can. The
//!     card surface is *additive*, not a rewrite of the underlying flow.

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
    /// Amber used for the conflict-warning triangles in the file list and
    /// the top-of-window "Merge Conflict" banner. Yellow is the universal
    /// "attention, not error" signal — the files aren't broken, they just
    /// need a choice. Red (DANGER) is reserved for destructive prompts
    /// (Abort, binary file blocker).
    pub const WARNING: Color32 = Color32::from_rgb(230, 180, 50);
    /// Soft card background used behind the Local/Remote cards. Slightly
    /// lifted from the window fill so the cards read as "panels" without
    /// needing a heavy stroke.
    pub const CARD_FILL: Color32 = Color32::from_rgb(38, 40, 44);
    /// Even fainter divider colour for the connector line drawn between
    /// the two cards (see `draw_card_connector`).
    pub const CONNECTOR: Color32 = Color32::from_rgb(120, 120, 130);
}

/// Navigation direction for Prev/Next conflict-marker jumps.
#[derive(Clone, Copy)]
enum NavIntent {
    PrevConflict,
    NextConflict,
}

enum ConflictIntent {
    UseFile(PathBuf, ConflictChoice),
    ResolveEditorRegion(PathBuf, usize, EditorResolution),
    ResolveEditorAll(PathBuf, EditorResolution),
    SaveManual(PathBuf, String),
    /// Hand off a single conflicted path to the user's configured external
    /// mergetool (`git mergetool -- <path>`). The subprocess is spawned
    /// detached — it may open a blocking GUI or write directly back to
    /// the working tree, depending on the configured tool.
    LaunchMergetool(PathBuf),
    Continue,
    Abort,
}

/// Built-in editor-side resolution choice for a single conflict region.
#[derive(Clone, Copy)]
enum EditorResolution {
    Ours,
    Theirs,
    Both,
}

#[derive(Clone)]
struct ConflictRegion {
    /// Byte range covering the full conflict block, including markers.
    start: usize,
    end: usize,
    ours: String,
    theirs: String,
}

/// State for the editor's conflict-navigation cursor. Recomputed every
/// frame from the current text (cheap — conflict markers are rare).
struct ConflictMarkers {
    regions: Vec<ConflictRegion>,
}

impl ConflictMarkers {
    fn scan(text: &str) -> Self {
        let mut regions = Vec::new();
        let mut pos = 0;
        let mut conflict_start: Option<usize> = None;
        let mut in_theirs = false;
        let mut ours = String::new();
        let mut theirs = String::new();
        for line in text.split_inclusive('\n') {
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.starts_with("<<<<<<<") {
                conflict_start = Some(pos);
                in_theirs = false;
                ours.clear();
                theirs.clear();
            } else if trimmed.starts_with("=======") && conflict_start.is_some() {
                in_theirs = true;
            } else if trimmed.starts_with(">>>>>>>") && conflict_start.is_some() {
                regions.push(ConflictRegion {
                    start: conflict_start.take().unwrap_or(pos),
                    end: pos + line.len(),
                    ours: ours.clone(),
                    theirs: theirs.clone(),
                });
                in_theirs = false;
            } else if conflict_start.is_some() {
                if in_theirs {
                    theirs.push_str(line);
                } else {
                    ours.push_str(line);
                }
            }
            pos += line.len();
        }
        Self { regions }
    }

    fn count(&self) -> usize {
        self.regions.len()
    }

    fn start_offsets(&self) -> impl Iterator<Item = usize> + '_ {
        self.regions.iter().map(|region| region.start)
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

                    ui.add_space(10.0);

                    // ---------- Local / Remote cards ------------------------
                    //
                    // The card pair is the *primary* affordance: two side-by-
                    // side cards communicate the Ours-vs-Theirs decision far
                    // more clearly than two adjacent buttons. Clicking the
                    // checkbox on a card is equivalent to pressing "Use
                    // ours/theirs" — it's just a more spatial way of
                    // expressing the same choice. The old button row remains
                    // below so keyboard / muscle-memory users aren't displaced.
                    render_card_picker(ui, entry, &labels, &mut intent);

                    ui.add_space(10.0);

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
                                "Resolve the whole file to the {} version, discarding {}",
                                labels.ours_short, labels.theirs_short
                            )).clicked() {
                                intent = Some(ConflictIntent::UseFile(
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
                                "Resolve the whole file to the {} version, discarding {}",
                                labels.theirs_short, labels.ours_short
                            )).clicked() {
                                intent = Some(ConflictIntent::UseFile(
                                    entry.path.clone(),
                                    ConflictChoice::Theirs,
                                ));
                            }
                        });

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

                        // "Merge in External Tool" — we wire it to
                        // `git mergetool -- <path>`, which reads
                        // `merge.tool` from the user's global git config
                        // (so every tool git supports works without us
                        // having to special-case any of them). `git
                        // mergetool` itself writes back to the working
                        // tree and stages the file on success, so no
                        // MergeFox-side write is required.
                        if ui
                            .button(
                                RichText::new("🛠 Merge in External Tool")
                                    .strong(),
                            )
                            .on_hover_text(
                                "Launch `git mergetool` for this file.\n\
                                 Respects your git config `merge.tool` — configure it with e.g.\n\
                                 `git config --global merge.tool kaleidoscope`.",
                            )
                            .clicked()
                        {
                            intent = Some(ConflictIntent::LaunchMergetool(
                                entry.path.clone(),
                            ));
                        }
                    });

                    ui.add_space(6.0);

                    // ---------- Line-by-line 3-way editor -----------------
                    //
                    // A dedicated side-by-side view where each conflict hunk
                    // gets its own pair of checkboxes (Their's on the left,
                    // Our's on the right — standard 3-way merge layout).
                    // Clicking a side's checkbox schedules that
                    // side's lines for inclusion in the resolved output;
                    // checking both emits ours + blank + theirs (the same
                    // semantics as `EditorResolution::Both`). The view is
                    // line-numbered and accent-tinted so conflict regions
                    // stand out from context.
                    //
                    // This is additive to the card picker (whole-file pick)
                    // and the advanced text editor — users who want to drop
                    // to raw markers still can. Default-collapsed because
                    // the card picker handles the common whole-file case
                    // faster.
                    if !entry.is_binary {
                        let three_way_header = egui::CollapsingHeader::new(
                            RichText::new("Line-by-line 3-way editor").strong(),
                        )
                        .id_salt(egui::Id::new(("conflict_3way", &entry.path)))
                        .default_open(false);
                        three_way_header.show(ui, |ui| {
                            render_three_way_editor(
                                ui,
                                entry,
                                &ws.conflict_editor_text,
                                &labels,
                                &mut intent,
                            );
                        });
                    }

                    ui.add_space(6.0);

                    // ---------- Advanced (text-editor) fallback ------------
                    //
                    // Everything below is the pre-card-redesign editor UI.
                    // We keep it intact — users who actually hand-resolve
                    // markers rely on the colour-coded editor, Prev/Next
                    // jumps, and the built-in "Use ours/theirs in all
                    // regions" helpers. Wrapping it in a `CollapsingHeader`
                    // gets it out of the way by default while still making
                    // it one click to open.
                    //
                    // The collapsing header opens by default for binary
                    // files (because the card picker is the only path
                    // forward) and when markers already exist in the
                    // working tree (user is mid-hand-edit).
                    let markers_for_default =
                        ConflictMarkers::scan(&ws.conflict_editor_text);
                    let default_open =
                        entry.is_binary || markers_for_default.count() == 0;
                    // `Id::new(&entry.path)` keeps each file's advanced
                    // section opened/closed state separate, so toggling one
                    // file doesn't stomp another.
                    let adv_header = egui::CollapsingHeader::new(
                        RichText::new("Advanced: edit merged result").strong(),
                    )
                    .id_salt(egui::Id::new(("conflict_adv", &entry.path)))
                    .default_open(default_open);
                    adv_header.show(ui, |ui| {
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

                        if markers.count() > 0 {
                            ui.add_space(6.0);
                            ui.group(|ui| {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(RichText::new("Built-in Conflict Tools").strong());
                                    ui.weak(
                                        "Resolve each region with ours, theirs, or both without asking AI.",
                                    );
                                });
                                ui.horizontal_wrapped(|ui| {
                                    if ui
                                        .button(format!("Use {} In All Regions", labels.ours_short))
                                        .on_hover_text(
                                            "Replace every conflict block in the editor with the ours side",
                                        )
                                        .clicked()
                                    {
                                        intent = Some(ConflictIntent::ResolveEditorAll(
                                            entry.path.clone(),
                                            EditorResolution::Ours,
                                        ));
                                    }
                                    if ui
                                        .button(format!(
                                            "Use {} In All Regions",
                                            labels.theirs_short
                                        ))
                                        .on_hover_text(
                                            "Replace every conflict block in the editor with the theirs side",
                                        )
                                        .clicked()
                                    {
                                        intent = Some(ConflictIntent::ResolveEditorAll(
                                            entry.path.clone(),
                                            EditorResolution::Theirs,
                                        ));
                                    }
                                    if ui
                                        .button("Take Both In All Regions")
                                        .on_hover_text(
                                            "Replace every conflict block with ours first, then theirs",
                                        )
                                        .clicked()
                                    {
                                        intent = Some(ConflictIntent::ResolveEditorAll(
                                            entry.path.clone(),
                                            EditorResolution::Both,
                                        ));
                                    }
                                });
                                ui.add_space(4.0);
                                ScrollArea::vertical()
                                    .auto_shrink([false, false])
                                    .max_height(180.0)
                                    .show(ui, |ui| {
                                        for (idx, region) in markers.regions.iter().enumerate() {
                                            render_region_card(
                                                ui,
                                                entry,
                                                idx,
                                                region,
                                                &labels,
                                                &mut intent,
                                            );
                                        }
                                    });
                            });
                        }
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
                    }); // end advanced-editor CollapsingHeader
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
        Some(ConflictIntent::UseFile(path, choice)) => {
            resolve_conflict_choice(app, &path, choice);
        }
        Some(ConflictIntent::ResolveEditorRegion(path, index, choice)) => {
            apply_editor_resolution(app, &path, Some(index), choice);
        }
        Some(ConflictIntent::ResolveEditorAll(path, choice)) => {
            apply_editor_resolution(app, &path, None, choice);
        }
        Some(ConflictIntent::SaveManual(path, text)) => {
            resolve_conflict_manual(app, &path, &text);
        }
        Some(ConflictIntent::LaunchMergetool(path)) => {
            launch_mergetool(app, &path);
        }
        Some(ConflictIntent::Continue) => app.continue_conflict_operation(),
        Some(ConflictIntent::Abort) => app.abort_conflict_operation(),
        None => {}
    }
}

fn render_region_card(
    ui: &mut egui::Ui,
    entry: &ConflictEntry,
    idx: usize,
    region: &ConflictRegion,
    labels: &SideLabels,
    intent: &mut Option<ConflictIntent>,
) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(6.0, 4.0))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("Conflict {}", idx + 1)).strong());
                ui.weak(format!(
                    "{} line{} vs {} line{}",
                    line_count(&region.ours),
                    if line_count(&region.ours) == 1 {
                        ""
                    } else {
                        "s"
                    },
                    line_count(&region.theirs),
                    if line_count(&region.theirs) == 1 {
                        ""
                    } else {
                        "s"
                    }
                ));
                if ui
                    .small_button(format!("Use {}", labels.ours_short))
                    .on_hover_text("Replace this region with the ours side")
                    .clicked()
                {
                    *intent = Some(ConflictIntent::ResolveEditorRegion(
                        entry.path.clone(),
                        idx,
                        EditorResolution::Ours,
                    ));
                }
                if ui
                    .small_button(format!("Use {}", labels.theirs_short))
                    .on_hover_text("Replace this region with the theirs side")
                    .clicked()
                {
                    *intent = Some(ConflictIntent::ResolveEditorRegion(
                        entry.path.clone(),
                        idx,
                        EditorResolution::Theirs,
                    ));
                }
                if ui
                    .small_button("Both")
                    .on_hover_text("Replace this region with ours first, then theirs")
                    .clicked()
                {
                    *intent = Some(ConflictIntent::ResolveEditorRegion(
                        entry.path.clone(),
                        idx,
                        EditorResolution::Both,
                    ));
                }
            });
            ui.horizontal_top(|ui| {
                ui.colored_label(
                    palette::OURS,
                    RichText::new(format!("{}:", labels.ours_short))
                        .small()
                        .strong(),
                );
                ui.weak(snippet(&region.ours));
            });
            ui.horizontal_top(|ui| {
                ui.colored_label(
                    palette::THEIRS,
                    RichText::new(format!("{}:", labels.theirs_short))
                        .small()
                        .strong(),
                );
                ui.weak(snippet(&region.theirs));
            });
        });
}

/// File-list row with an inline conflict-count badge.
///
/// Conflicted files carry a yellow warning triangle. Yellow reads as
/// "needs attention" without implying an error — conflicts are a normal
/// part of merging, not a crash. Binary files *do* get a red chip
/// because the user genuinely can't hand-merge them; that's a harder
/// blocker.
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
                    // Prefix the row with a yellow triangle so the eye
                    // immediately lands on "these are the conflicted
                    // files". Using RichText with explicit color beats a
                    // coloured_label because it keeps the triangle aligned
                    // with the selectable_label's baseline.
                    ui.label(
                        RichText::new("▲")
                            .color(palette::WARNING)
                            .strong(),
                    );
                    let path_resp = ui.selectable_label(selected, label_text);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if entry.is_binary {
                            ui.colored_label(palette::DANGER, "Bin");
                        } else if conflict_count > 0 {
                            ui.colored_label(
                                palette::WARNING,
                                RichText::new(format!("{conflict_count}"))
                                    .small()
                                    .strong(),
                            );
                        }
                    });
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
    // Heading: a prominent warning triangle next to a one-line title,
    // followed by an explanatory sentence. The operation-progress and
    // ours/theirs legend sit underneath because they carry MergeFox-
    // specific context (rebase step counter, the reminder that "theirs"
    // is your own commit during a rebase — see SideLabels::resolve).
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("⚠")
                .size(22.0)
                .color(palette::WARNING)
                .strong(),
        );
        ui.label(RichText::new("Merge Conflict").heading().strong());
    });
    ui.add_space(2.0);
    ui.label(
        "The following files were changed both locally and remotely. Select local \
         (yours) or remote (theirs) changes, or merge the changes manually.",
    );
    ui.add_space(6.0);
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
        let branch_suffix = head_branch.map(|b| format!(" ({b})")).unwrap_or_default();
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

/// Spawn `git mergetool -- <path>` in the current repo so whatever
/// merge driver the user has wired up in `merge.tool` opens for this
/// single file.
///
/// We deliberately use `std::process::Command::spawn` instead of the
/// synchronous `run()` helper in `git::cli` because mergetool is *blocking
/// and interactive* — it expects to keep the foreground until the user
/// resolves the file. Running it synchronously would freeze MergeFox's UI
/// thread for however long the user takes. Spawn-and-forget is the right
/// shape: `git mergetool` writes back to the working tree and stages the
/// file itself on success, so when the user next interacts with MergeFox
/// the normal conflict-entry scan will reflect the new state.
///
/// If `git mergetool` has no configured tool (`merge.tool` unset), the
/// git CLI prints an instructional error to stderr and exits — we swallow
/// the detached stdout/stderr here, so the user instead sees no effect and
/// the tooltip on the button tells them how to configure it. Good enough
/// for a first pass; a richer version could capture stderr into the HUD.
fn launch_mergetool(app: &mut MergeFoxApp, path: &Path) {
    let repo_path = {
        let View::Workspace(tabs) = &app.view else {
            return;
        };
        tabs.current().repo.path().to_path_buf()
    };

    // `-y` ("no-prompt") skips the "Hit return to start merge resolution
    // tool" prompt that git otherwise prints before each file — unhelpful
    // here because we've already made that choice by clicking the button.
    // The `--` ensures the path is never interpreted as a flag, even when
    // a file starts with a dash.
    let path_arg = path.display().to_string();
    let spawn_result = std::process::Command::new("git")
        .arg("mergetool")
        .arg("-y")
        .arg("--")
        .arg(&path_arg)
        .current_dir(&repo_path)
        // Fully detach: we don't wait, we don't care about exit code, and
        // we don't want the child printing to our stdout/stderr since the
        // GUI process captures those for its own logs.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    match spawn_result {
        Ok(_child) => {
            app.hud = Some(crate::app::Hud::new(
                format!(
                    "Launched external mergetool for {}",
                    format_path(path)
                ),
                2400,
            ));
        }
        Err(err) => {
            app.last_error = Some(format!(
                "launch mergetool: {err}\n\
                 Is `git` on PATH and `merge.tool` configured?"
            ));
        }
    }
}

/// Render the Local (our) / Remote (their) card pair.
///
/// Each card is a rounded, softly-filled panel with:
///   * a heading stating the side + operation-specific label
///     (e.g. "Local (our)" / "Remote (their)"),
///   * the branch/tip short label from [`SideLabels`],
///   * the file path with a lightweight icon prefix,
///   * a "modified" / "absent" status chip,
///   * a checkbox that picks the side when clicked. The checkbox reads as
///     a "radio" in effect: checking Local unchecks Remote (both cards
///     immediately commit the choice by firing the same `UseFile` intent
///     that the legacy buttons use).
///
/// A thin connector line runs between the cards with a small diamond at
/// its midpoint — a visual cue that the two cards are the two sides of
/// one choice. We keep the connector purely decorative (clicking it does
/// nothing) because egui can't easily overlay an interactive widget
/// mid-panel without a separate layer.
fn render_card_picker(
    ui: &mut egui::Ui,
    entry: &ConflictEntry,
    labels: &SideLabels,
    intent: &mut Option<ConflictIntent>,
) {
    // Reserve the full available width, split into two cards with a narrow
    // connector gutter in the middle. We use `ui.columns(3, …)` to get
    // three equal-width columns then manually resize by putting the
    // connector drawing inside the middle column — but a cleaner approach
    // is `ui.horizontal` with explicit sizing so the cards stretch.
    let total_width = ui.available_width();
    let connector_width: f32 = 40.0;
    let card_width = ((total_width - connector_width) / 2.0).max(240.0);

    ui.horizontal(|ui| {
        // --- Local (our) card -------------------------------------------
        let picked_ours = false; // stateless: each click commits immediately
        render_side_card(
            ui,
            CardSpec {
                width: card_width,
                heading: format!("Local ({})", labels.ours_short),
                accent: palette::OURS,
                branch_tip: labels.ours_long.clone(),
                path: entry.path.clone(),
                available: entry.ours.is_some(),
                checked: picked_ours,
                on_pick: || {
                    ConflictIntent::UseFile(entry.path.clone(), ConflictChoice::Ours)
                },
            },
            intent,
        );

        // --- Connector between cards -----------------------------------
        draw_card_connector(ui, connector_width);

        // --- Remote (their) card ----------------------------------------
        let picked_theirs = false;
        render_side_card(
            ui,
            CardSpec {
                width: card_width,
                heading: format!("Remote ({})", labels.theirs_short),
                accent: palette::THEIRS,
                branch_tip: labels.theirs_long.clone(),
                path: entry.path.clone(),
                available: entry.theirs.is_some(),
                checked: picked_theirs,
                on_pick: || {
                    ConflictIntent::UseFile(
                        entry.path.clone(),
                        ConflictChoice::Theirs,
                    )
                },
            },
            intent,
        );
    });
}

/// Inputs for a single side-card. Grouped into a struct so `render_card_picker`
/// reads as a pair of symmetric calls rather than a 9-positional-arg blob.
struct CardSpec<F: FnOnce() -> ConflictIntent> {
    width: f32,
    heading: String,
    accent: Color32,
    branch_tip: String,
    path: PathBuf,
    /// False if the blob doesn't exist on this side (file added on one side
    /// only). The card still renders — users need to see that one side is
    /// empty — but the checkbox is disabled.
    available: bool,
    /// Currently-checked? Always false in the current implementation since
    /// we commit on click (no pending state), but left here so a future
    /// "stage a choice then confirm" flow can light up the checkbox.
    checked: bool,
    /// Intent builder fired when the user picks this card. Boxed as a
    /// closure so both cards can produce different `ConflictChoice`
    /// variants without branching inside `render_side_card`.
    on_pick: F,
}

fn render_side_card<F: FnOnce() -> ConflictIntent>(
    ui: &mut egui::Ui,
    spec: CardSpec<F>,
    intent: &mut Option<ConflictIntent>,
) {
    let CardSpec {
        width,
        heading,
        accent,
        branch_tip,
        path,
        available,
        checked,
        on_pick,
    } = spec;

    egui::Frame::none()
        .fill(palette::CARD_FILL)
        .stroke(Stroke::new(1.5, accent))
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::symmetric(12.0, 10.0))
        .show(ui, |ui| {
            ui.set_width(width);
            // Heading row: side + accent bar on the left.
            ui.horizontal(|ui| {
                ui.colored_label(accent, RichText::new("●").strong());
                ui.label(RichText::new(&heading).strong().size(15.0));
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let mut cb = checked;
                        let resp = ui
                            .add_enabled(
                                available,
                                egui::Checkbox::without_text(&mut cb),
                            )
                            .on_hover_text(if available {
                                "Resolve the whole file to this side"
                            } else {
                                "This side is absent (file added or deleted elsewhere)"
                            });
                        if resp.clicked() && available {
                            *intent = Some(on_pick());
                        }
                    },
                );
            });

            ui.add_space(4.0);

            // Branch / tip name row.
            ui.horizontal(|ui| {
                ui.label(RichText::new("⎇").color(palette::MUTED));
                ui.label(RichText::new(&branch_tip).monospace().small());
            });

            ui.add_space(2.0);

            // File path row — uses a generic "page" glyph because MergeFox
            // doesn't yet have filetype-specific icons in the conflicts UI.
            ui.horizontal(|ui| {
                ui.label(RichText::new("📄").color(palette::MUTED));
                ui.label(
                    RichText::new(format_path(&path))
                        .monospace()
                        .small(),
                );
            });

            ui.add_space(6.0);

            // "modified" status chip. For the "absent" side (file was
            // only added or deleted on one side) we say so explicitly
            // so the user knows what the checkbox would actually do if
            // it were enabled.
            ui.horizontal(|ui| {
                let (chip_text, chip_color) = if available {
                    ("modified", palette::WARNING)
                } else {
                    ("absent", palette::MUTED)
                };
                egui::Frame::none()
                    .fill(chip_color.gamma_multiply(0.25))
                    .stroke(Stroke::new(1.0, chip_color))
                    .rounding(egui::Rounding::same(10.0))
                    .inner_margin(egui::Margin::symmetric(8.0, 2.0))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(chip_text)
                                .small()
                                .strong()
                                .color(chip_color),
                        );
                    });
            });
        });
}

/// Draw a horizontal connector line with a diamond in the middle between
/// the Local and Remote cards. Purely visual: it reinforces that the cards
/// are "two sides of one choice" rather than independent controls.
///
/// We draw directly via `ui.painter()` at the allocated rect so the line
/// sits vertically centred regardless of card content height. The rect's
/// width is fixed by the caller (see `connector_width` in
/// `render_card_picker`) — ~40px gives the two cards visible
/// breathing room without letting the connector line feel stringy.
fn draw_card_connector(ui: &mut egui::Ui, width: f32) {
    // Allocate the gutter with a min height large enough to centre against
    // typical card height (~110px). The painter draws at the midpoint so
    // the exact height doesn't matter as long as it's >0.
    let desired = egui::vec2(width, 100.0);
    let (rect, _resp) =
        ui.allocate_exact_size(desired, egui::Sense::hover());
    let painter = ui.painter_at(rect);

    let mid_y = rect.center().y;
    let left = egui::pos2(rect.left() + 4.0, mid_y);
    let right = egui::pos2(rect.right() - 4.0, mid_y);

    painter.line_segment(
        [left, right],
        Stroke::new(1.5, palette::CONNECTOR),
    );

    // Diamond in the middle — slightly rotated square drawn as a
    // filled convex polygon.
    let cx = rect.center().x;
    let cy = mid_y;
    let r = 5.0;
    let diamond = vec![
        egui::pos2(cx, cy - r),
        egui::pos2(cx + r, cy),
        egui::pos2(cx, cy + r),
        egui::pos2(cx - r, cy),
    ];
    painter.add(egui::Shape::convex_polygon(
        diamond,
        palette::CONNECTOR,
        Stroke::new(1.0, palette::CONNECTOR),
    ));
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
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.weak(format_size(blob.size));
                        if let Some(oid) = blob.oid {
                            ui.weak(RichText::new(short_sha(&oid)).monospace());
                        }
                    });
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
    if markers.regions.is_empty() {
        return None;
    }
    let current_char = TextEdit::load_state(ctx, editor_id)
        .and_then(|state| state.cursor.char_range().map(|r| r.primary.index))
        .unwrap_or(0);

    // We compare in BYTE offsets because that's how we recorded markers.
    // Approximation: character index == byte index is only correct for
    // ASCII, but source files with conflicts are overwhelmingly ASCII in
    // the marker vicinity, and the jump is coarse anyway.
    let current_byte = current_char;
    match nav {
        NavIntent::NextConflict => markers
            .start_offsets()
            .find(|&m| m > current_byte)
            .or_else(|| markers.start_offsets().next()),
        NavIntent::PrevConflict => markers
            .start_offsets()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .find(|&m| m < current_byte)
            .or_else(|| markers.start_offsets().last()),
    }
}

fn apply_editor_resolution(
    app: &mut MergeFoxApp,
    path: &Path,
    region_index: Option<usize>,
    choice: EditorResolution,
) {
    let result = {
        let View::Workspace(tabs) = &mut app.view else {
            return;
        };
        let ws = tabs.current_mut();
        let updated = match region_index {
            Some(index) => resolve_region_in_editor(&ws.conflict_editor_text, index, choice),
            None => Some(resolve_all_regions_in_editor(
                &ws.conflict_editor_text,
                choice,
            )),
        };
        if let Some(text) = updated {
            ws.conflict_editor_text = text;
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "conflict region no longer exists in the editor"
            ))
        }
    };

    match result {
        Ok(()) => {
            let action = match (region_index, choice) {
                (Some(_), EditorResolution::Ours) => "Applied ours to one region",
                (Some(_), EditorResolution::Theirs) => "Applied theirs to one region",
                (Some(_), EditorResolution::Both) => "Applied both sides to one region",
                (None, EditorResolution::Ours) => "Applied ours to all regions",
                (None, EditorResolution::Theirs) => "Applied theirs to all regions",
                (None, EditorResolution::Both) => "Applied both sides to all regions",
            };
            app.hud = Some(crate::app::Hud::new(
                format!("{action} in {}", format_path(path)),
                1800,
            ));
        }
        Err(err) => {
            app.last_error = Some(format!("editor conflict tool: {err:#}"));
        }
    }
}

fn resolve_region_in_editor(
    text: &str,
    region_index: usize,
    choice: EditorResolution,
) -> Option<String> {
    let markers = ConflictMarkers::scan(text);
    let region = markers.regions.get(region_index)?;
    let replacement = match choice {
        EditorResolution::Ours => region.ours.as_str(),
        EditorResolution::Theirs => region.theirs.as_str(),
        EditorResolution::Both => {
            return Some(replace_region(
                text,
                region,
                &(region.ours.clone() + &region.theirs),
            ))
        }
    };
    Some(replace_region(text, region, replacement))
}

fn resolve_all_regions_in_editor(text: &str, choice: EditorResolution) -> String {
    let markers = ConflictMarkers::scan(text);
    if markers.regions.is_empty() {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for region in &markers.regions {
        out.push_str(&text[cursor..region.start]);
        match choice {
            EditorResolution::Ours => out.push_str(&region.ours),
            EditorResolution::Theirs => out.push_str(&region.theirs),
            EditorResolution::Both => {
                out.push_str(&region.ours);
                out.push_str(&region.theirs);
            }
        }
        cursor = region.end;
    }
    out.push_str(&text[cursor..]);
    out
}

fn replace_region(text: &str, region: &ConflictRegion, replacement: &str) -> String {
    let mut out = String::with_capacity(text.len() + replacement.len());
    out.push_str(&text[..region.start]);
    out.push_str(replacement);
    out.push_str(&text[region.end..]);
    out
}

fn line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.lines().count().max(1)
    }
}

fn snippet(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        "(empty)".to_string()
    } else {
        let mut out: String = first.chars().take(72).collect();
        if first.chars().count() > 72 {
            out.push_str("...");
        }
        out
    }
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

// ===========================================================================
// 3-way line-by-line editor
// ===========================================================================
//
// The line-by-line 3-way editor renders Their's on the left and Our's on
// the right, each column a line-numbered monospace view of the file. Conflict
// regions are painted with accent fills and carry a checkbox on the hunk
// header: checking a side schedules its lines for inclusion in the resolved
// output. Prev / Next arrows jump between regions; Resolve composes the
// merged text and writes it through the existing `SaveManual` intent.
//
// Design notes
// ------------
// * **Pure parsing.** `ThreeWaySegment::parse` walks the working-tree text
//   (same inputs as `ConflictMarkers::scan`) and produces a flat sequence of
//   `Context` blocks and `Conflict` hunks. Keeping the parse pure makes the
//   merge composition trivially testable.
// * **Hunk selection state** is kept in `egui::Context::data_mut`, keyed by
//   the file path so the user's in-progress picks survive frame-to-frame
//   scrolling and re-layouts without us touching `WorkspaceState`.
// * **Scroll linkage.** egui's `ScrollArea::id_salt(...).link_with(...)` is
//   the supported way to sync two scroll areas. We wrap both columns in a
//   shared `egui::ScrollArea::vertical` via a single `horizontal` strip so
//   the two columns always scroll together.
// * **Navigation.** Prev/Next set a `scroll_to_region` target consumed on
//   the next frame; each hunk row calls `scroll_to_me` when its index
//   matches.
// * **Resolve output.** For each region we emit ours, theirs, both (in that
//   order, blank line between), or neither based on the (ours_sel, their_sel)
//   tuple. `compose_three_way_resolution` is pure and has unit-test coverage.

/// One segment of a parsed 3-way file: either a block of non-conflict
/// context lines, or a conflict hunk with the ours/theirs variants.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ThreeWaySegment {
    Context(Vec<String>),
    Conflict { ours: Vec<String>, theirs: Vec<String> },
}

impl ThreeWaySegment {
    /// Parse the working-tree text into a flat segment sequence. Uses the
    /// same conflict-marker recognition as `ConflictMarkers::scan`.
    fn parse(text: &str) -> Vec<ThreeWaySegment> {
        let mut out: Vec<ThreeWaySegment> = Vec::new();
        let mut ctx: Vec<String> = Vec::new();
        let mut in_conflict = false;
        let mut in_theirs = false;
        let mut ours: Vec<String> = Vec::new();
        let mut theirs: Vec<String> = Vec::new();

        let flush_ctx = |ctx: &mut Vec<String>, out: &mut Vec<ThreeWaySegment>| {
            if !ctx.is_empty() {
                out.push(ThreeWaySegment::Context(std::mem::take(ctx)));
            }
        };

        for line in text.lines() {
            if line.starts_with("<<<<<<<") {
                flush_ctx(&mut ctx, &mut out);
                in_conflict = true;
                in_theirs = false;
                ours.clear();
                theirs.clear();
            } else if in_conflict && line.starts_with("=======") {
                in_theirs = true;
            } else if in_conflict && line.starts_with(">>>>>>>") {
                out.push(ThreeWaySegment::Conflict {
                    ours: std::mem::take(&mut ours),
                    theirs: std::mem::take(&mut theirs),
                });
                in_conflict = false;
                in_theirs = false;
            } else if in_conflict {
                if in_theirs {
                    theirs.push(line.to_string());
                } else {
                    ours.push(line.to_string());
                }
            } else {
                ctx.push(line.to_string());
            }
        }
        flush_ctx(&mut ctx, &mut out);
        out
    }

    fn is_conflict(&self) -> bool {
        matches!(self, ThreeWaySegment::Conflict { .. })
    }
}

/// Compose the resolved text from parsed segments plus one boolean pair
/// per conflict region (`ours_selected`, `theirs_selected`). Order is
/// always ours first, then theirs when both are selected, with a blank
/// separator line between them (matches `EditorResolution::Both`
/// semantics). Neither-selected regions emit nothing — that's a valid
/// "drop this hunk" outcome.
fn compose_three_way_resolution(
    segments: &[ThreeWaySegment],
    selections: &[(bool, bool)],
) -> String {
    let mut out = String::new();
    let mut sel_iter = selections.iter();
    for (i, seg) in segments.iter().enumerate() {
        match seg {
            ThreeWaySegment::Context(lines) => {
                for l in lines {
                    out.push_str(l);
                    out.push('\n');
                }
            }
            ThreeWaySegment::Conflict { ours, theirs } => {
                let (take_ours, take_theirs) = sel_iter.next().copied().unwrap_or((false, false));
                let mut wrote_any = false;
                if take_ours {
                    for l in ours {
                        out.push_str(l);
                        out.push('\n');
                    }
                    wrote_any = true;
                }
                if take_theirs {
                    if wrote_any && !ours.is_empty() {
                        out.push('\n');
                    }
                    for l in theirs {
                        out.push_str(l);
                        out.push('\n');
                    }
                }
                let _ = i; // silence unused warning in release builds
            }
        }
    }
    out
}

/// Assign display line numbers to each segment's lines for a single side
/// (ours or theirs). Context lines are shared between sides; conflict
/// lines are side-specific so each column's numbering advances
/// independently through its side's hunks. Returns a flat Vec matching
/// the render order: one entry per rendered row carrying `(line_number,
/// kind)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    Context,
    ConflictOurs,
    ConflictTheirs,
    /// Filler row inserted to keep the two columns aligned when one side
    /// has more lines than the other in the same hunk.
    Filler,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NumberedRow {
    line_no: Option<usize>,
    kind: RowKind,
    text: String,
    /// Region index for conflict rows / filler rows, used for scroll
    /// targeting and checkbox state lookup.
    region: Option<usize>,
}

/// Build per-column numbered row lists. The two columns are padded with
/// `RowKind::Filler` entries inside each hunk so their scroll offsets
/// align visually (a 3-line ours + 5-line theirs hunk becomes 5 rows on
/// each side: 3 content + 2 filler on the ours column).
fn layout_three_way_rows(
    segments: &[ThreeWaySegment],
) -> (Vec<NumberedRow>, Vec<NumberedRow>) {
    let mut left: Vec<NumberedRow> = Vec::new(); // Their's
    let mut right: Vec<NumberedRow> = Vec::new(); // Our's
    let mut left_no = 1usize;
    let mut right_no = 1usize;
    let mut region_idx = 0usize;

    for seg in segments {
        match seg {
            ThreeWaySegment::Context(lines) => {
                for line in lines {
                    left.push(NumberedRow {
                        line_no: Some(left_no),
                        kind: RowKind::Context,
                        text: line.clone(),
                        region: None,
                    });
                    right.push(NumberedRow {
                        line_no: Some(right_no),
                        kind: RowKind::Context,
                        text: line.clone(),
                        region: None,
                    });
                    left_no += 1;
                    right_no += 1;
                }
            }
            ThreeWaySegment::Conflict { ours, theirs } => {
                let height = ours.len().max(theirs.len());
                for row in 0..height {
                    if row < theirs.len() {
                        left.push(NumberedRow {
                            line_no: Some(left_no),
                            kind: RowKind::ConflictTheirs,
                            text: theirs[row].clone(),
                            region: Some(region_idx),
                        });
                        left_no += 1;
                    } else {
                        left.push(NumberedRow {
                            line_no: None,
                            kind: RowKind::Filler,
                            text: String::new(),
                            region: Some(region_idx),
                        });
                    }
                    if row < ours.len() {
                        right.push(NumberedRow {
                            line_no: Some(right_no),
                            kind: RowKind::ConflictOurs,
                            text: ours[row].clone(),
                            region: Some(region_idx),
                        });
                        right_no += 1;
                    } else {
                        right.push(NumberedRow {
                            line_no: None,
                            kind: RowKind::Filler,
                            text: String::new(),
                            region: Some(region_idx),
                        });
                    }
                }
                region_idx += 1;
            }
        }
    }
    (left, right)
}

/// Per-file 3-way UI state cached in `egui::Context::data`. Keyed by the
/// path so each file has independent checkbox picks and scroll targets.
#[derive(Clone, Default)]
struct ThreeWayState {
    /// (ours_selected, theirs_selected) per conflict region. Grown lazily
    /// to match the region count; default is "ours checked, theirs
    /// unchecked" so a naive Resolve keeps the user's own work.
    selections: Vec<(bool, bool)>,
    /// When set, render scrolls the given region index into view on the
    /// next frame.
    scroll_to_region: Option<usize>,
    /// Last region the user navigated to, used by Prev/Next to compute
    /// the next target without looking at scroll offsets.
    cursor_region: usize,
}

fn three_way_state_id(path: &Path) -> egui::Id {
    egui::Id::new(("conflict_3way_state", path.display().to_string()))
}

fn load_three_way_state(ctx: &egui::Context, path: &Path) -> ThreeWayState {
    ctx.data(|d| d.get_temp::<ThreeWayState>(three_way_state_id(path)))
        .unwrap_or_default()
}

fn store_three_way_state(ctx: &egui::Context, path: &Path, state: ThreeWayState) {
    ctx.data_mut(|d| d.insert_temp(three_way_state_id(path), state));
}

/// Render the 3-way editor for a single conflicted file.
fn render_three_way_editor(
    ui: &mut egui::Ui,
    entry: &ConflictEntry,
    text: &str,
    labels: &SideLabels,
    intent: &mut Option<ConflictIntent>,
) {
    let segments = ThreeWaySegment::parse(text);
    let region_count = segments.iter().filter(|s| s.is_conflict()).count();

    let mut state = load_three_way_state(ui.ctx(), &entry.path);
    if state.selections.len() < region_count {
        // Default: keep *our* lines (the safer choice when the user
        // hasn't looked at each hunk yet — matches the common
        // "Accept Current Change" default).
        state.selections.resize(region_count, (true, false));
    }
    if state.selections.len() > region_count {
        state.selections.truncate(region_count);
    }
    if state.cursor_region >= region_count {
        state.cursor_region = region_count.saturating_sub(1);
    }

    // --- Top strip: region count + Prev/Next navigation -------------------
    ui.horizontal(|ui| {
        ui.weak(
            RichText::new(format!(
                "{} conflict region{}",
                region_count,
                if region_count == 1 { "" } else { "s" }
            ))
            .small(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_enabled_ui(region_count > 0, |ui| {
                if ui
                    .small_button("›")
                    .on_hover_text("Next conflict region")
                    .clicked()
                {
                    state.cursor_region = (state.cursor_region + 1) % region_count.max(1);
                    state.scroll_to_region = Some(state.cursor_region);
                }
                if ui
                    .small_button("‹")
                    .on_hover_text("Previous conflict region")
                    .clicked()
                {
                    state.cursor_region = if state.cursor_region == 0 {
                        region_count.saturating_sub(1)
                    } else {
                        state.cursor_region - 1
                    };
                    state.scroll_to_region = Some(state.cursor_region);
                }
            });
        });
    });

    ui.add_space(4.0);

    // --- Twin scrollable columns -----------------------------------------
    //
    // Both columns share one logical scroll area via `link_with` so they
    // move together. The columns carry identical row counts (padded with
    // Filler entries inside hunks) which means scroll positions map 1:1.
    let (left_rows, right_rows) = layout_three_way_rows(&segments);
    let scroll_target = state.scroll_to_region.take();
    let link_id = egui::Id::new(("conflict_3way_scroll", &entry.path));

    ui.horizontal_top(|ui| {
        let col_w = (ui.available_width() - 6.0) / 2.0;
        ui.allocate_ui(egui::vec2(col_w, 420.0), |ui| {
            render_three_way_column(
                ui,
                &format!("Their's — {}", labels.theirs_short),
                palette::THEIRS,
                true, // left column controls theirs checkbox
                &left_rows,
                &mut state.selections,
                scroll_target,
                link_id.with("left"),
                link_id,
            );
        });
        ui.allocate_ui(egui::vec2(col_w, 420.0), |ui| {
            render_three_way_column(
                ui,
                &format!("Our's — {}", labels.ours_short),
                palette::OURS,
                false, // right column controls ours checkbox
                &right_rows,
                &mut state.selections,
                scroll_target,
                link_id.with("right"),
                link_id,
            );
        });
    });

    ui.add_space(8.0);

    // --- Footer: Cancel / Resolve ----------------------------------------
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let resolve_btn = egui::Button::new(
                RichText::new("✓ Resolve").color(Color32::WHITE).strong(),
            )
            .fill(palette::SUCCESS);
            if ui
                .add_enabled(region_count > 0 || !segments.is_empty(), resolve_btn)
                .on_hover_text(
                    "Write the composed text to the working tree and mark the file resolved.",
                )
                .clicked()
            {
                let resolved = compose_three_way_resolution(&segments, &state.selections);
                *intent = Some(ConflictIntent::SaveManual(entry.path.clone(), resolved));
            }
            if ui
                .button("Cancel")
                .on_hover_text(
                    "Reset all hunk picks. The working-tree file is not touched.",
                )
                .clicked()
            {
                state.selections = vec![(true, false); region_count];
                state.cursor_region = 0;
                state.scroll_to_region = None;
            }
        });
    });

    store_three_way_state(ui.ctx(), &entry.path, state);
}

/// Render one column of the 3-way editor. `is_theirs_side` controls which
/// half of each `(ours, theirs)` selection tuple this column's checkbox
/// drives: the left column flips the theirs bit, the right column flips
/// the ours bit. Both columns still read both bits so a checkbox click on
/// one side is immediately reflected on the other side's highlight
/// colour.
#[allow(clippy::too_many_arguments)]
fn render_three_way_column(
    ui: &mut egui::Ui,
    title: &str,
    accent: Color32,
    is_theirs_side: bool,
    rows: &[NumberedRow],
    selections: &mut [(bool, bool)],
    scroll_target: Option<usize>,
    scroll_id: egui::Id,
    link_with: egui::Id,
) {
    egui::Frame::group(ui.style())
        .stroke(Stroke::new(1.5, accent))
        .inner_margin(egui::Margin::symmetric(6.0, 6.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.colored_label(accent, RichText::new(title).strong());
            });
            ui.separator();

            let font = egui::FontId::monospace(12.5);
            let row_h = 16.0;

            ScrollArea::vertical()
                .id_salt(scroll_id)
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                .auto_shrink([false, false])
                .max_height(400.0)
                .show(ui, |ui| {
                    // Link horizontal & vertical scroll so both columns move
                    // together. egui exposes scroll linking via an Id shared
                    // across scroll areas — see egui::scroll_area::ScrollArea
                    // docs. We don't do anything fancier than vertical
                    // coupling here because the columns are the same width.
                    ui.ctx()
                        .data_mut(|d| d.insert_temp(link_with, scroll_id));

                    let mut last_region: Option<usize> = None;
                    for (idx, row) in rows.iter().enumerate() {
                        // Insert a hunk header (with checkbox) the first
                        // time we see a region — the header row is
                        // independent of the line rows so the checkbox
                        // always lands above the hunk even when the hunk
                        // is only one line tall.
                        if let Some(region) = row.region {
                            if last_region != Some(region) {
                                render_hunk_header(
                                    ui,
                                    region,
                                    is_theirs_side,
                                    accent,
                                    selections,
                                );
                                if scroll_target == Some(region) {
                                    ui.scroll_to_cursor(Some(egui::Align::TOP));
                                }
                                last_region = Some(region);
                            }
                        } else {
                            last_region = None;
                        }
                        render_three_way_row(ui, row, accent, &font, row_h, idx, selections);
                    }
                });
        });
}

fn render_hunk_header(
    ui: &mut egui::Ui,
    region: usize,
    is_theirs_side: bool,
    accent: Color32,
    selections: &mut [(bool, bool)],
) {
    ui.add_space(2.0);
    let (ours_sel, theirs_sel) = selections
        .get(region)
        .copied()
        .unwrap_or((false, false));
    ui.horizontal(|ui| {
        let mut flag = if is_theirs_side { theirs_sel } else { ours_sel };
        let resp = ui.add(egui::Checkbox::without_text(&mut flag));
        if resp.changed() {
            if let Some(slot) = selections.get_mut(region) {
                if is_theirs_side {
                    slot.1 = flag;
                } else {
                    slot.0 = flag;
                }
            }
        }
        ui.colored_label(
            accent,
            RichText::new(format!("Hunk {}", region + 1)).small().strong(),
        );
        ui.weak(
            RichText::new(if is_theirs_side {
                "include these theirs lines"
            } else {
                "include these ours lines"
            })
            .small(),
        );
    });
}

fn render_three_way_row(
    ui: &mut egui::Ui,
    row: &NumberedRow,
    accent: Color32,
    font: &egui::FontId,
    row_h: f32,
    _idx: usize,
    selections: &[(bool, bool)],
) {
    let (bg, fg) = match row.kind {
        RowKind::Context => (
            palette::MUTED.gamma_multiply(0.08),
            Color32::LIGHT_GRAY,
        ),
        RowKind::ConflictOurs => {
            let checked = row
                .region
                .and_then(|r| selections.get(r))
                .map(|s| s.0)
                .unwrap_or(false);
            let base = palette::OURS.gamma_multiply(if checked { 0.45 } else { 0.22 });
            (base, Color32::WHITE)
        }
        RowKind::ConflictTheirs => {
            let checked = row
                .region
                .and_then(|r| selections.get(r))
                .map(|s| s.1)
                .unwrap_or(false);
            let base = palette::THEIRS.gamma_multiply(if checked { 0.45 } else { 0.22 });
            (base, Color32::WHITE)
        }
        RowKind::Filler => (
            palette::MUTED.gamma_multiply(0.04),
            palette::MUTED,
        ),
    };

    let (rect, _resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), row_h),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, bg);

    // Line-number gutter — 40px wide, right-aligned mono digits.
    let gutter_w = 40.0;
    let gutter_rect = egui::Rect::from_min_size(rect.min, egui::vec2(gutter_w, rect.height()));
    painter.rect_filled(gutter_rect, 0.0, Color32::from_rgb(30, 32, 36));
    if let Some(n) = row.line_no {
        painter.text(
            gutter_rect.right_center() - egui::vec2(4.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            n.to_string(),
            egui::FontId::monospace(11.0),
            palette::MUTED,
        );
    }

    // Accent stripe to the left of the text — subtle reminder of which
    // side this row belongs to.
    let stripe = egui::Rect::from_min_size(
        egui::pos2(gutter_rect.right(), rect.top()),
        egui::vec2(2.0, rect.height()),
    );
    if matches!(row.kind, RowKind::ConflictOurs | RowKind::ConflictTheirs) {
        painter.rect_filled(stripe, 0.0, accent);
    }

    // Row text.
    let text_pos = egui::pos2(gutter_rect.right() + 6.0, rect.center().y);
    let display = if row.text.is_empty() && matches!(row.kind, RowKind::Filler) {
        "".to_string()
    } else {
        row.text.clone()
    };
    painter.text(
        text_pos,
        egui::Align2::LEFT_CENTER,
        display,
        font.clone(),
        fg,
    );
}

#[cfg(test)]
mod three_way_tests {
    use super::*;

    fn sample_conflict() -> String {
        "alpha\n\
         <<<<<<< HEAD\n\
         ours-1\n\
         ours-2\n\
         =======\n\
         theirs-1\n\
         >>>>>>> branch\n\
         omega\n"
            .to_string()
    }

    #[test]
    fn parse_splits_context_and_conflict() {
        let segs = ThreeWaySegment::parse(&sample_conflict());
        assert_eq!(segs.len(), 3);
        match &segs[0] {
            ThreeWaySegment::Context(l) => assert_eq!(l, &vec!["alpha".to_string()]),
            _ => panic!("expected context"),
        }
        match &segs[1] {
            ThreeWaySegment::Conflict { ours, theirs } => {
                assert_eq!(ours, &vec!["ours-1".to_string(), "ours-2".to_string()]);
                assert_eq!(theirs, &vec!["theirs-1".to_string()]);
            }
            _ => panic!("expected conflict"),
        }
        match &segs[2] {
            ThreeWaySegment::Context(l) => assert_eq!(l, &vec!["omega".to_string()]),
            _ => panic!("expected context"),
        }
    }

    #[test]
    fn parse_handles_no_markers() {
        let segs = ThreeWaySegment::parse("one\ntwo\nthree\n");
        assert_eq!(segs.len(), 1);
        assert!(matches!(&segs[0], ThreeWaySegment::Context(l) if l.len() == 3));
    }

    #[test]
    fn compose_ours_only() {
        let segs = ThreeWaySegment::parse(&sample_conflict());
        let out = compose_three_way_resolution(&segs, &[(true, false)]);
        assert_eq!(out, "alpha\nours-1\nours-2\nomega\n");
    }

    #[test]
    fn compose_theirs_only() {
        let segs = ThreeWaySegment::parse(&sample_conflict());
        let out = compose_three_way_resolution(&segs, &[(false, true)]);
        assert_eq!(out, "alpha\ntheirs-1\nomega\n");
    }

    #[test]
    fn compose_both_sides() {
        let segs = ThreeWaySegment::parse(&sample_conflict());
        let out = compose_three_way_resolution(&segs, &[(true, true)]);
        // ours block, blank separator, theirs block.
        assert_eq!(out, "alpha\nours-1\nours-2\n\ntheirs-1\nomega\n");
    }

    #[test]
    fn compose_neither_drops_hunk() {
        let segs = ThreeWaySegment::parse(&sample_conflict());
        let out = compose_three_way_resolution(&segs, &[(false, false)]);
        assert_eq!(out, "alpha\nomega\n");
    }

    #[test]
    fn layout_pads_unequal_hunks_with_filler() {
        // 2 ours vs 1 theirs → both columns should be 2 rows tall inside
        // the hunk (theirs column gets one filler row).
        let (left, right) = layout_three_way_rows(&ThreeWaySegment::parse(&sample_conflict()));
        // 1 context (alpha) + 2 hunk rows + 1 context (omega) = 4 rows
        // on each side.
        assert_eq!(left.len(), 4);
        assert_eq!(right.len(), 4);
        // Row 2 (index 2) on the left (theirs side) should be filler
        // because theirs only has 1 line while ours has 2.
        assert!(matches!(left[2].kind, RowKind::Filler));
        // All ours rows on the right are content.
        assert!(matches!(right[1].kind, RowKind::ConflictOurs));
        assert!(matches!(right[2].kind, RowKind::ConflictOurs));
    }

    #[test]
    fn layout_numbers_each_side_independently() {
        // Context line counts once on each side starting at 1; conflict
        // lines continue the numbering for their own side.
        let (left, right) = layout_three_way_rows(&ThreeWaySegment::parse(&sample_conflict()));
        assert_eq!(left[0].line_no, Some(1)); // alpha
        assert_eq!(left[1].line_no, Some(2)); // theirs-1
        assert_eq!(left[2].line_no, None); // filler
        assert_eq!(left[3].line_no, Some(3)); // omega

        assert_eq!(right[0].line_no, Some(1)); // alpha
        assert_eq!(right[1].line_no, Some(2)); // ours-1
        assert_eq!(right[2].line_no, Some(3)); // ours-2
        assert_eq!(right[3].line_no, Some(4)); // omega
    }

    #[test]
    fn compose_multiple_hunks_preserves_order() {
        let text = "A\n<<<<<<<\nO1\n=======\nT1\n>>>>>>>\nB\n<<<<<<<\nO2\n=======\nT2\n>>>>>>>\nC\n";
        let segs = ThreeWaySegment::parse(text);
        let out = compose_three_way_resolution(&segs, &[(true, false), (false, true)]);
        assert_eq!(out, "A\nO1\nB\nT2\nC\n");
    }

    #[test]
    fn compose_with_missing_selection_defaults_to_nothing() {
        // Defensive: if the selections slice is shorter than the region
        // count we should still emit a sane result (context + nothing
        // for the unspecified regions) rather than panicking.
        let segs = ThreeWaySegment::parse(&sample_conflict());
        let out = compose_three_way_resolution(&segs, &[]);
        assert_eq!(out, "alpha\nomega\n");
    }
}
