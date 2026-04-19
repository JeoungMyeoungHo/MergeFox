//! Split-commit wizard modal.
//!
//! Opens against one target commit, enumerates its hunks via
//! `git::discover_hunks`, and lets the user assign each hunk to one
//! of N "parts" (each part becoming a new commit). On confirm, the
//! caller (`app.rs`) kicks off `git::split_commit` on a worker thread;
//! this module owns only the dialog state + rendering.
//!
//! The v1 granularity restriction enforced by `git::split_commit`
//! (no file may straddle parts) is re-enforced here at the UI level:
//! picking "part 2" for one hunk of `foo.rs` forces every other hunk
//! of `foo.rs` into part 2 too. That keeps the user from ever
//! producing an invalid plan and means the Split button disables on
//! the real reason ("still assigning hunks") rather than on a
//! "can't split file mid-way" surprise.
//!
//! Layout
//! ------
//! Two columns inside an `egui::Window`:
//!   * Left: "Available hunks" — scrollable list, one row per hunk,
//!     with a header line + preview lines.
//!   * Right: "Parts" — scrollable list of N new commits, each with
//!     a message input, an inline hunk list, and per-file controls.
//!
//! Per-hunk assignment uses a compact combo-box ("Part 1 / Part 2 /
//! …"), which is simpler than drag-and-drop and survives keyboard
//! navigation. An "+ Add part" button appends a fresh empty part.
//! The primary "Split" action is gated by:
//!   * ≥2 parts
//!   * every part has a non-empty message
//!   * every hunk assigned to some part
//!   * no file straddles parts (enforced by the per-file forcing
//!     logic, so this is normally automatic)

use std::collections::BTreeMap;
use std::path::PathBuf;

use egui::{Align, Align2, Color32, Key, RichText, ScrollArea};

use crate::git::{DiscoveredHunk, HunkRef, SplitPart, SplitPlan};

/// Persistent state for the split-commit modal. Lives on
/// `MergeFoxApp::split_commit_modal` (Option-wrapped: `None` = closed).
#[derive(Debug, Clone)]
pub struct SplitCommitModalState {
    /// OID of the commit we're splitting.
    pub target_oid: gix::ObjectId,
    /// Short display SHA + subject line — pure display.
    pub header_short_oid: String,
    pub header_subject: String,
    /// Discovered hunks (oldest-first per file, files in diff order).
    pub hunks: Vec<DiscoveredHunk>,
    /// One entry per "part" the user is building. Index into this vec
    /// matches the Part N label in the UI (Part 1 = index 0).
    pub parts: Vec<PartDraft>,
    /// For each hunk (indexed the same as `self.hunks`), which part
    /// it's currently assigned to. `None` = unassigned. On open we
    /// place everything in part 0 so the dialog has an obvious
    /// "split off part 2" starting shape.
    pub assignment: Vec<Option<usize>>,
}

/// A single part-in-progress. The user will see this as "Part N: …".
#[derive(Debug, Clone, Default)]
pub struct PartDraft {
    pub message: String,
}

/// The user's decision this frame.
#[derive(Debug, Clone)]
pub enum SplitCommitModalOutcome {
    /// Build the final plan and start the worker.
    Confirmed(SplitPlan),
    /// Dismiss without touching git.
    Cancelled,
}

impl SplitCommitModalState {
    /// Build a fresh modal with the given hunks, assigning them all
    /// to part 0 by default. At least two parts exist at open time so
    /// the user can start moving hunks immediately without clicking
    /// "Add part" first.
    pub fn new(
        target_oid: gix::ObjectId,
        header_short_oid: String,
        header_subject: String,
        hunks: Vec<DiscoveredHunk>,
    ) -> Self {
        let assignment = vec![Some(0); hunks.len()];
        Self {
            target_oid,
            header_short_oid,
            header_subject,
            hunks,
            parts: vec![
                PartDraft {
                    message: String::new(),
                },
                PartDraft {
                    message: String::new(),
                },
            ],
            assignment,
        }
    }

    /// True when the plan is internally valid and the Split button
    /// should light up.
    pub fn can_confirm(&self) -> bool {
        if self.parts.len() < 2 {
            return false;
        }
        if self.parts.iter().any(|p| p.message.trim().is_empty()) {
            return false;
        }
        if self.assignment.iter().any(|a| a.is_none()) {
            return false;
        }
        // Every part must receive at least one hunk — otherwise it'd
        // produce an empty commit, which the v1 backend rejects.
        let mut touched = vec![false; self.parts.len()];
        for a in &self.assignment {
            if let Some(p) = a {
                if *p < touched.len() {
                    touched[*p] = true;
                }
            }
        }
        touched.iter().all(|&t| t)
    }

    /// Human-readable reason the Split button is disabled, or `None`
    /// if the plan is ready.
    pub fn blocking_reason(&self) -> Option<String> {
        if self.parts.len() < 2 {
            return Some("Add at least two parts.".into());
        }
        let missing = self.assignment.iter().filter(|a| a.is_none()).count();
        if missing > 0 {
            return Some(format!(
                "{missing} hunk(s) not yet assigned to a part."
            ));
        }
        for (idx, p) in self.parts.iter().enumerate() {
            if p.message.trim().is_empty() {
                return Some(format!("Part {} needs a commit message.", idx + 1));
            }
        }
        let mut touched = vec![false; self.parts.len()];
        for a in &self.assignment {
            if let Some(p) = a {
                if *p < touched.len() {
                    touched[*p] = true;
                }
            }
        }
        for (idx, t) in touched.iter().enumerate() {
            if !*t {
                return Some(format!("Part {} has no hunks.", idx + 1));
            }
        }
        None
    }

    /// Assign `hunk_idx` to `target_part`, pulling every OTHER hunk
    /// of the same file along with it. This enforces the v1
    /// "whole-file-per-part" rule at the UI layer so the backend
    /// validation never fires.
    pub fn assign_hunk(&mut self, hunk_idx: usize, target_part: usize) {
        if hunk_idx >= self.hunks.len() || target_part >= self.parts.len() {
            return;
        }
        let file = self.hunks[hunk_idx].file.clone();
        for (i, h) in self.hunks.iter().enumerate() {
            if h.file == file {
                self.assignment[i] = Some(target_part);
            }
        }
    }

    /// Append a fresh empty part. Limits runaway UI growth at 20 —
    /// sane humans don't split into 20 commits, and the picker scales
    /// poorly past that.
    pub fn add_part(&mut self) {
        if self.parts.len() < 20 {
            self.parts.push(PartDraft::default());
        }
    }

    /// Remove part `idx`. Assignments pointing at it reset to `None`
    /// (user must re-assign), and assignments pointing at indices >
    /// `idx` shift down by one to stay in sync.
    pub fn remove_part(&mut self, idx: usize) {
        if idx >= self.parts.len() || self.parts.len() <= 2 {
            return;
        }
        self.parts.remove(idx);
        for a in self.assignment.iter_mut() {
            match *a {
                Some(p) if p == idx => *a = None,
                Some(p) if p > idx => *a = Some(p - 1),
                _ => {}
            }
        }
    }

    /// Build the final `SplitPlan` from the current assignment.
    /// Returns `None` if the plan isn't ready; callers should gate on
    /// `can_confirm()` first.
    pub fn to_plan(&self) -> Option<SplitPlan> {
        if !self.can_confirm() {
            return None;
        }
        // Group hunks by assigned part in part-order-then-hunk-order
        // so parts[0] comes out oldest-first.
        let mut buckets: BTreeMap<usize, Vec<HunkRef>> = BTreeMap::new();
        for (i, h) in self.hunks.iter().enumerate() {
            if let Some(p) = self.assignment[i] {
                buckets.entry(p).or_default().push(HunkRef {
                    file: h.file.clone(),
                    hunk_index: h.hunk_index,
                });
            }
        }
        let mut parts = Vec::with_capacity(self.parts.len());
        for (idx, draft) in self.parts.iter().enumerate() {
            let hunks = buckets.remove(&idx).unwrap_or_default();
            parts.push(SplitPart {
                message: draft.message.clone(),
                hunks,
            });
        }
        Some(SplitPlan {
            target_oid: self.target_oid,
            parts,
        })
    }
}

/// Render the modal. Returns `Some(outcome)` when the user clicked
/// a terminal button; the caller then clears the state. Safe to call
/// every frame.
pub fn show(
    ctx: &egui::Context,
    state: &mut SplitCommitModalState,
) -> Option<SplitCommitModalOutcome> {
    let mut outcome: Option<SplitCommitModalOutcome> = None;

    egui::Window::new("Split commit")
        .title_bar(true)
        .collapsible(false)
        .resizable(true)
        .anchor(Align2::CENTER_TOP, [0.0, 64.0])
        .default_size([860.0, 560.0])
        .min_width(720.0)
        .min_height(420.0)
        .show(ctx, |ui| {
            // ---- Header: target commit identity ----
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("Splitting {}", state.header_short_oid))
                        .strong()
                        .color(Color32::from_rgb(220, 220, 230)),
                );
                ui.separator();
                ui.label(RichText::new(&state.header_subject).italics());
            });

            ui.add_space(4.0);
            ui.weak(
                "Each hunk of the original commit goes into exactly one new commit below. \
                 Hunks of the same file must land together (v1 restriction — assigning one \
                 hunk pulls its siblings along automatically).",
            );
            ui.add_space(10.0);

            // ---- Two-column body ----
            let available_w = ui.available_width();
            let col_w = (available_w * 0.5) - 8.0;

            ui.horizontal_top(|ui| {
                // Left column: available hunks.
                ui.vertical(|ui| {
                    ui.set_width(col_w);
                    ui.label(RichText::new("Hunks").strong());
                    ui.add_space(2.0);
                    render_hunk_list(ui, state);
                });

                ui.separator();

                // Right column: parts.
                ui.vertical(|ui| {
                    ui.set_width(col_w);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Parts").strong());
                        ui.with_layout(
                            egui::Layout::right_to_left(Align::Center),
                            |ui| {
                                if ui.button("+ Add part").clicked() {
                                    state.add_part();
                                }
                            },
                        );
                    });
                    ui.add_space(2.0);
                    render_parts_list(ui, state);
                });
            });

            ui.add_space(10.0);
            ui.separator();

            // ---- Blocking-reason hint + action row ----
            let reason = state.blocking_reason();
            ui.horizontal(|ui| {
                if let Some(r) = &reason {
                    ui.label(
                        RichText::new(format!("⚠ {r}"))
                            .color(Color32::from_rgb(220, 180, 100)),
                    );
                } else {
                    ui.label(
                        RichText::new("Ready to split.")
                            .color(Color32::from_rgb(140, 200, 140)),
                    );
                }

                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    // Destructive-action fill colour (matches other
                    // rewrite dialogs in the app).
                    let split_btn = egui::Button::new(
                        RichText::new("Split commit")
                            .color(Color32::from_rgb(255, 240, 230))
                            .strong(),
                    )
                    .fill(Color32::from_rgb(212, 92, 92));
                    let enabled = reason.is_none();
                    if ui.add_enabled(enabled, split_btn).clicked() {
                        if let Some(plan) = state.to_plan() {
                            outcome = Some(SplitCommitModalOutcome::Confirmed(plan));
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        outcome = Some(SplitCommitModalOutcome::Cancelled);
                    }
                });
            });
        });

    // Esc closes the dialog cleanly. We deliberately do NOT bind
    // Ctrl+Enter to confirm — the plan is complex and a stray chord
    // shouldn't trigger a history rewrite.
    if ctx.input(|i| i.key_pressed(Key::Escape)) {
        outcome = Some(SplitCommitModalOutcome::Cancelled);
    }

    outcome
}

/// Left-column hunk list: one row per hunk with file · header ·
/// preview. Non-interactive aside from hover tooltips — assignment
/// happens via the combo on the right.
fn render_hunk_list(ui: &mut egui::Ui, state: &SplitCommitModalState) {
    if state.hunks.is_empty() {
        ui.weak(
            "No splittable hunks in this commit (likely a binary-only \
             or empty commit).",
        );
        return;
    }
    ScrollArea::vertical()
        .id_source("split-hunks")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut last_file: Option<PathBuf> = None;
            for (i, h) in state.hunks.iter().enumerate() {
                // File heading — printed once per file.
                if last_file.as_ref() != Some(&h.file) {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(display_path(&h.file))
                            .strong()
                            .color(Color32::from_rgb(180, 200, 220)),
                    );
                    last_file = Some(h.file.clone());
                }

                let assigned_to = state.assignment[i];
                let (status_text, status_colour) = match assigned_to {
                    Some(p) => (
                        format!("→ Part {}", p + 1),
                        Color32::from_rgb(140, 200, 160),
                    ),
                    None => (
                        "unassigned".to_string(),
                        Color32::from_rgb(220, 140, 140),
                    ),
                };

                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("  #{}", h.hunk_index + 1))
                            .monospace()
                            .weak(),
                    );
                    ui.label(
                        RichText::new(truncate(&h.header, 60))
                            .monospace()
                            .color(Color32::from_rgb(200, 200, 210)),
                    );
                    ui.with_layout(
                        egui::Layout::right_to_left(Align::Center),
                        |ui| {
                            ui.label(
                                RichText::new(status_text)
                                    .monospace()
                                    .color(status_colour),
                            );
                        },
                    );
                })
                .response
                .on_hover_text(format!(
                    "{}\n\n{}",
                    h.header,
                    if h.preview.is_empty() {
                        "(no preview)"
                    } else {
                        &h.preview
                    }
                ));
            }
        });
}

/// Right-column: one collapsible section per part, each with a
/// message editor and the list of hunks currently assigned to it
/// (plus an "add hunk…" selector that moves a hunk into this part).
fn render_parts_list(ui: &mut egui::Ui, state: &mut SplitCommitModalState) {
    ScrollArea::vertical()
        .id_source("split-parts")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Snapshot the part count so we don't observe mutation
            // mid-loop if the user clicks "remove" — we apply the
            // removal after the loop.
            let part_count = state.parts.len();
            let mut remove_request: Option<usize> = None;

            for idx in 0..part_count {
                ui.add_space(4.0);
                egui::Frame::none()
                    .fill(Color32::from_rgb(34, 38, 46))
                    .rounding(6.0)
                    .inner_margin(egui::Margin::same(8.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("Part {}", idx + 1)).strong(),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(Align::Center),
                                |ui| {
                                    if part_count > 2
                                        && ui
                                            .small_button("✕ remove part")
                                            .clicked()
                                    {
                                        remove_request = Some(idx);
                                    }
                                },
                            );
                        });

                        let msg_edit = egui::TextEdit::multiline(
                            &mut state.parts[idx].message,
                        )
                        .hint_text("Commit message for this part")
                        .desired_rows(2)
                        .desired_width(f32::INFINITY);
                        ui.add(msg_edit);

                        ui.add_space(4.0);
                        ui.label(RichText::new("Hunks in this part").weak());

                        // List of hunks currently assigned to `idx`.
                        // We snapshot (file, hunk_index) into a local
                        // so the inner ComboBox closure can mutably
                        // borrow `state` without clashing with the
                        // outer immutable borrow into `state.hunks`.
                        let mut has_any = false;
                        let assigned: Vec<(usize, PathBuf, usize)> = (0..state.hunks.len())
                            .filter(|&i| state.assignment[i] == Some(idx))
                            .map(|i| {
                                let h = &state.hunks[i];
                                (i, h.file.clone(), h.hunk_index)
                            })
                            .collect();
                        for (i, file, hunk_index) in assigned {
                            has_any = true;
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "{} #{}",
                                        display_path(&file),
                                        hunk_index + 1
                                    ))
                                    .monospace()
                                    .color(Color32::from_rgb(200, 220, 210)),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(Align::Center),
                                    |ui| {
                                        render_reassign_menu(ui, state, i, idx);
                                    },
                                );
                            });
                        }
                        if !has_any {
                            ui.weak("(no hunks yet — pick from unassigned below)");
                        }
                    });
            }

            if let Some(idx) = remove_request {
                state.remove_part(idx);
            }

            // ---- Unassigned / re-assign section ----
            ui.add_space(8.0);
            ui.separator();
            ui.label(RichText::new("Unassigned hunks").strong());
            let mut unassigned_any = false;
            let unassigned: Vec<(usize, PathBuf, usize)> = (0..state.hunks.len())
                .filter(|&i| state.assignment[i].is_none())
                .map(|i| {
                    let h = &state.hunks[i];
                    (i, h.file.clone(), h.hunk_index)
                })
                .collect();
            for (i, file, hunk_index) in unassigned {
                unassigned_any = true;
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} #{}",
                            display_path(&file),
                            hunk_index + 1
                        ))
                        .monospace(),
                    );
                    ui.with_layout(
                        egui::Layout::right_to_left(Align::Center),
                        |ui| {
                            render_reassign_menu(ui, state, i, usize::MAX);
                        },
                    );
                });
            }
            if !unassigned_any {
                ui.weak("(every hunk is assigned)");
            }
        });
}

/// Render a compact "Move to Part N" dropdown for one hunk. `current`
/// is the currently-assigned part index (use `usize::MAX` to mean
/// "unassigned"). Selecting a new part re-runs `assign_hunk`, which
/// pulls sibling hunks of the same file along automatically.
fn render_reassign_menu(
    ui: &mut egui::Ui,
    state: &mut SplitCommitModalState,
    hunk_idx: usize,
    current: usize,
) {
    let label = if current == usize::MAX {
        "Move to…".to_string()
    } else {
        format!("Part {} ▾", current + 1)
    };
    let part_count = state.parts.len();
    let mut chosen: Option<usize> = None;
    egui::ComboBox::from_id_source(("split-part-sel", hunk_idx))
        .selected_text(label)
        .width(90.0)
        .show_ui(ui, |ui| {
            for p in 0..part_count {
                if ui
                    .selectable_label(current == p, format!("Part {}", p + 1))
                    .clicked()
                {
                    chosen = Some(p);
                }
            }
        });
    if let Some(p) = chosen {
        state.assign_hunk(hunk_idx, p);
    }
}

fn display_path(p: &std::path::Path) -> String {
    p.to_string_lossy().into_owned()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_state() -> SplitCommitModalState {
        let oid = gix::ObjectId::null(gix::hash::Kind::Sha1);
        let hunks = vec![
            DiscoveredHunk {
                file: PathBuf::from("a.rs"),
                hunk_index: 0,
                header: "@@ -1,1 +1,1 @@".into(),
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                preview: String::new(),
            },
            DiscoveredHunk {
                file: PathBuf::from("a.rs"),
                hunk_index: 1,
                header: "@@ -5,1 +5,1 @@".into(),
                old_start: 5,
                old_count: 1,
                new_start: 5,
                new_count: 1,
                preview: String::new(),
            },
            DiscoveredHunk {
                file: PathBuf::from("b.rs"),
                hunk_index: 0,
                header: "@@ -1,1 +1,1 @@".into(),
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                preview: String::new(),
            },
        ];
        SplitCommitModalState::new(oid, "abc1234".into(), "subject".into(), hunks)
    }

    #[test]
    fn assign_hunk_drags_file_siblings() {
        let mut s = mk_state();
        // Start: all three on part 0. Move a.rs#0 to part 1 → a.rs#1
        // follows, b.rs#0 stays on part 0.
        s.assign_hunk(0, 1);
        assert_eq!(s.assignment, vec![Some(1), Some(1), Some(0)]);
    }

    #[test]
    fn remove_part_resets_assignments_and_shifts_higher_parts() {
        let mut s = mk_state();
        s.add_part();
        s.add_part();
        // assignments: 0,0,0. Move b.rs to part 2. Shape: [0,0,2].
        s.assign_hunk(2, 2);
        assert_eq!(s.assignment, vec![Some(0), Some(0), Some(2)]);
        s.remove_part(1);
        // Part 1 had nothing → no resets; part 2 shifts down to part 1.
        assert_eq!(s.assignment, vec![Some(0), Some(0), Some(1)]);
    }

    #[test]
    fn can_confirm_requires_messages_and_full_coverage() {
        let mut s = mk_state();
        assert!(!s.can_confirm()); // messages empty.
        s.parts[0].message = "one".into();
        s.parts[1].message = "two".into();
        // All hunks currently on part 0 → part 1 is empty → blocked.
        assert!(!s.can_confirm());
        // Move b.rs#0 to part 1 and we should be ready.
        s.assign_hunk(2, 1);
        assert!(s.can_confirm());
    }

    #[test]
    fn to_plan_emits_parts_in_order() {
        let mut s = mk_state();
        s.parts[0].message = "first".into();
        s.parts[1].message = "second".into();
        s.assign_hunk(2, 1);
        let plan = s.to_plan().expect("plan should be ready");
        assert_eq!(plan.parts.len(), 2);
        assert_eq!(plan.parts[0].message, "first");
        assert_eq!(plan.parts[1].message, "second");
        assert_eq!(plan.parts[0].hunks.len(), 2); // a.rs x2
        assert_eq!(plan.parts[1].hunks.len(), 1); // b.rs x1
    }

    #[test]
    fn blocking_reason_names_missing_message() {
        let mut s = mk_state();
        s.parts[0].message = "".into();
        s.parts[1].message = "set".into();
        s.assign_hunk(2, 1);
        let reason = s.blocking_reason().unwrap();
        assert!(reason.contains("Part 1"), "reason={reason}");
    }
}
