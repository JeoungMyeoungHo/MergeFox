//! Per-hunk conflict editor.
//!
//! The raw-textarea editor in `conflicts.rs` works but is coarse — the user
//! has to find every `<<<<<<<` / `=======` / `>>>>>>>` themselves and
//! hand-delete markers without slipping. This view is the structural
//! alternative: each `Conflict` chunk in the parsed file is a discrete
//! card the user can resolve independently. Context chunks render read-only
//! in between.
//!
//! Why a separate module:
//!
//!   * `conflicts.rs` was already ~2500 lines of mixed concerns (status
//!     logic, card picker, three-way editor, raw textarea fallback). The
//!     per-hunk editor is self-contained enough — it only needs the
//!     parsed file plus the user's resolution slice — that pulling it out
//!     keeps the top-level file focused on orchestration.
//!   * The module exposes a single entry point, `render`, that the caller
//!     invokes once per frame. All interactive state (which hunk is
//!     active, what's typed in the custom textarea) either lives on
//!     `WorkspaceState` (persistent across frames) or in egui-side state
//!     keyed by a stable id (custom-editor open/closed, the textarea's
//!     cursor).
//!
//! Intents flow back to the caller through an owned `HunkIntent` value.
//! We do not call `Repo` APIs here — that's the caller's job — so this
//! module is easy to unit-test (no git subprocess, no filesystem) and
//! doesn't entangle the UI tree with mutable app state.

use egui::{Color32, RichText, ScrollArea, Stroke, TextEdit};

use crate::git::{
    default_custom_seed, ConflictHunkKind, HunkResolution, HunkResolutionState, ParsedConflict,
};

/// Alpha-blended backgrounds for the ours / theirs panes.
///
/// We use RGBA with mid-range alpha so the tint reads clearly on both
/// light and dark themes — a fully opaque fill would clash with the
/// window chrome in light mode. Values picked to harmonise with the
/// existing palette in `src/ui/conflicts.rs` (cool blue for ours, warm
/// orange for theirs). Kept here, not in `conflicts.rs`'s private
/// `palette` module, because the alpha levels are specific to the
/// tinted-card look this view wants.
// Color32::from_rgba_unmultiplied is not a const fn in this egui version;
// small helpers keep the call-site readable without per-frame cost
// (egui's Color32 copy is cheap).
fn bg_ours() -> Color32 {
    Color32::from_rgba_unmultiplied(120, 170, 220, 50)
}
fn bg_theirs() -> Color32 {
    Color32::from_rgba_unmultiplied(200, 140, 100, 50)
}
fn bg_ancestor() -> Color32 {
    Color32::from_rgba_unmultiplied(170, 170, 170, 35)
}
const PILL_PENDING: Color32 = Color32::from_rgb(230, 180, 50);
const PILL_OURS: Color32 = Color32::from_rgb(86, 156, 214);
const PILL_THEIRS: Color32 = Color32::from_rgb(220, 140, 60);
const PILL_BOTH: Color32 = Color32::from_rgb(120, 190, 140);
const PILL_CUSTOM: Color32 = Color32::from_rgb(200, 170, 230);

/// An action the user performed on a hunk. The UI layer turns these into
/// mutations on the caller's `HunkResolutionState` vector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkIntent {
    /// Set hunk `index` to a fixed resolution (Ours / Theirs / Both / Pending).
    Set {
        index: usize,
        resolution: HunkResolution,
    },
    /// Open the inline custom editor for hunk `index`. The seed is what
    /// the textarea should contain the first time it opens. Calling this
    /// while already open is a no-op at the caller (the textarea retains
    /// whatever the user typed).
    OpenCustom { index: usize, seed: String },
    /// The user typed in the custom textarea — update the stored text and
    /// flip resolution to `Custom`.
    EditCustom { index: usize, text: String },
    /// Collapse the inline custom editor without changing the resolution.
    CloseCustom { index: usize },
    /// Keyboard-navigation: move the active-hunk cursor forward/backward
    /// to the next/previous hunk whose resolution is still `Pending`.
    NavPending { forward: bool },
    /// Keyboard-navigation: the user pressed `1` / `2` / `3` with a hunk
    /// active — same as `Set` on the active hunk.
    SetActive { resolution: HunkResolution },
}

/// Render the per-hunk editor for `parsed`.
///
/// Arguments:
///   * `ui` — egui frame we paint into.
///   * `parsed` — the parser's view of the working-tree file. Must have at
///     least one `Conflict` chunk; callers in `conflicts.rs` check this
///     before calling and route to the raw-textarea fallback otherwise.
///   * `resolutions` — per-hunk state, indexed 1:1 with `Conflict` chunks
///     in `parsed.chunks`. Read-only here; intents flow back to the caller.
///   * `active_hunk` — which hunk is "focused" for keyboard shortcuts.
///     `None` means no keyboard cursor; the caller seeds this to `Some(0)`
///     as soon as a file is selected.
///   * `labels` — the short side-labels ("HEAD", "feature", etc.) used in
///     button text and column headers.
///   * `open_custom` — which hunk indices currently have their inline
///     custom editor expanded. Driven by the egui ctx storage so it
///     persists across frames.
///
/// Returns the list of intents produced this frame (usually zero or one;
/// clicking multiple buttons in one frame is allowed but rare).
pub fn render(
    ui: &mut egui::Ui,
    parsed: &ParsedConflict,
    resolutions: &[HunkResolutionState],
    active_hunk: Option<usize>,
    labels: &SideLabels,
    open_custom: &mut Vec<bool>,
) -> Vec<HunkIntent> {
    let mut intents = Vec::new();

    // Conflict-chunk indices only (skip Context when numbering).
    let total_conflicts = parsed.conflict_count();
    let resolved = resolutions
        .iter()
        .filter(|r| r.resolution != HunkResolution::Pending)
        .count();

    // Header strip — progress + reset.
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!(
                "Resolving {resolved} of {total_conflicts} hunk{}",
                if total_conflicts == 1 { "" } else { "s" }
            ))
            .strong(),
        );
        ui.weak(format!(
            "·  Pending {}  ·  Resolved {}",
            total_conflicts.saturating_sub(resolved),
            resolved
        ));
    });
    ui.add_space(4.0);

    // The list itself. We use a ScrollArea so long files (dozens of
    // hunks) don't blow the window height. Each Context chunk renders as
    // read-only monospace; each Conflict chunk as a tinted card.
    ScrollArea::vertical()
        .id_salt("conflict_hunks_scroll")
        .auto_shrink([false, false])
        .max_height(520.0)
        .show(ui, |ui| {
            let mut conflict_idx = 0usize;
            for chunk in &parsed.chunks {
                match chunk {
                    ConflictHunkKind::Context(text) => render_context(ui, text),
                    ConflictHunkKind::Conflict {
                        ours,
                        theirs,
                        ancestor,
                    } => {
                        let state = resolutions
                            .get(conflict_idx)
                            .cloned()
                            .unwrap_or_default();
                        let is_active = active_hunk == Some(conflict_idx);
                        let mut open_flag = open_custom
                            .get(conflict_idx)
                            .copied()
                            .unwrap_or(false);
                        render_conflict_card(
                            ui,
                            conflict_idx,
                            total_conflicts,
                            ours,
                            theirs,
                            ancestor.as_deref(),
                            &state,
                            is_active,
                            &mut open_flag,
                            chunk,
                            labels,
                            &mut intents,
                        );
                        if open_custom.len() <= conflict_idx {
                            open_custom.resize(conflict_idx + 1, false);
                        }
                        open_custom[conflict_idx] = open_flag;
                        conflict_idx += 1;
                    }
                }
            }
        });

    intents
}

/// Short side-labels — mirrors the struct of the same name in
/// `conflicts.rs`. Kept as a simple inert struct here so the hunks view
/// doesn't have to pull in the SideLabels type's other machinery. The
/// caller populates both fields from the `SideLabels::resolve` result.
pub struct SideLabels {
    pub ours_short: String,
    pub theirs_short: String,
}

// ---------------------------------------------------------------------------
// Context renderer
// ---------------------------------------------------------------------------

/// Read-only monospace block for a Context chunk. Rendered with a thin
/// muted background so the eye can follow the "this is unchanged"
/// boundaries without the context lines blending into the surrounding
/// chrome.
fn render_context(ui: &mut egui::Ui, text: &str) {
    if text.is_empty() {
        return;
    }
    egui::Frame::none()
        .fill(Color32::from_rgba_unmultiplied(128, 128, 128, 18))
        .inner_margin(egui::Margin::symmetric(6.0, 3.0))
        .show(ui, |ui| {
            ui.style_mut().override_text_style = Some(egui::TextStyle::Monospace);
            // Trim a single trailing newline for display so we don't draw
            // a blank row at the bottom of every context block.
            let display = text.strip_suffix('\n').unwrap_or(text);
            ui.label(display);
        });
    ui.add_space(2.0);
}

// ---------------------------------------------------------------------------
// Conflict card renderer
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn render_conflict_card(
    ui: &mut egui::Ui,
    idx: usize,
    total: usize,
    ours: &str,
    theirs: &str,
    ancestor: Option<&str>,
    state: &HunkResolutionState,
    is_active: bool,
    open_custom: &mut bool,
    chunk: &ConflictHunkKind,
    labels: &SideLabels,
    intents: &mut Vec<HunkIntent>,
) {
    // Card stroke — active hunk gets a brighter edge so the keyboard
    // cursor is visible at a glance even in long lists.
    let stroke_color = if is_active {
        Color32::from_rgb(230, 180, 50)
    } else {
        Color32::from_rgb(70, 70, 80)
    };
    let stroke_width = if is_active { 2.0 } else { 1.0 };

    egui::Frame::none()
        .stroke(Stroke::new(stroke_width, stroke_color))
        .inner_margin(egui::Margin::symmetric(8.0, 6.0))
        .outer_margin(egui::Margin {
            top: 4.0,
            bottom: 4.0,
            left: 0.0,
            right: 0.0,
        })
        .rounding(egui::Rounding::same(4.0))
        .show(ui, |ui| {
            // ----- Header row: label + status pill + buttons -----
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("Conflict {} / {total}", idx + 1)).strong(),
                );
                status_pill(ui, state.resolution);
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if ui
                            .small_button(format!("Use {}", labels.theirs_short))
                            .on_hover_text("Keep the theirs side for this hunk (shortcut: 2)")
                            .clicked()
                        {
                            intents.push(HunkIntent::Set {
                                index: idx,
                                resolution: HunkResolution::Theirs,
                            });
                        }
                        if ui
                            .small_button("Use both")
                            .on_hover_text(
                                "Keep both sides (ours first, then theirs) for this hunk (shortcut: 3)",
                            )
                            .clicked()
                        {
                            intents.push(HunkIntent::Set {
                                index: idx,
                                resolution: HunkResolution::Both,
                            });
                        }
                        if ui
                            .small_button(format!("Use {}", labels.ours_short))
                            .on_hover_text("Keep the ours side for this hunk (shortcut: 1)")
                            .clicked()
                        {
                            intents.push(HunkIntent::Set {
                                index: idx,
                                resolution: HunkResolution::Ours,
                            });
                        }
                        let custom_label = if *open_custom {
                            "Hide custom"
                        } else {
                            "Edit custom..."
                        };
                        if ui
                            .small_button(custom_label)
                            .on_hover_text(
                                "Write your own resolution for this hunk. \
                                 Typing flips the resolution to Custom.",
                            )
                            .clicked()
                        {
                            if *open_custom {
                                intents
                                    .push(HunkIntent::CloseCustom { index: idx });
                            } else {
                                intents.push(HunkIntent::OpenCustom {
                                    index: idx,
                                    seed: if state.custom_text.is_empty() {
                                        default_custom_seed(chunk, state.resolution)
                                    } else {
                                        state.custom_text.clone()
                                    },
                                });
                            }
                            *open_custom = !*open_custom;
                        }
                    },
                );
            });

            ui.add_space(4.0);

            // ----- Side panes: ours / theirs (+ ancestor if present) -----
            // We lay out ours on the left and theirs on the right, with the
            // ancestor (if any) tucked below. Two-column layout matches the
            // ambient left=local / right=remote convention the rest of the
            // app uses.
            ui.columns(2, |cols| {
                render_side_pane(&mut cols[0], &labels.ours_short, ours, bg_ours());
                render_side_pane(&mut cols[1], &labels.theirs_short, theirs, bg_theirs());
            });
            if let Some(anc) = ancestor {
                ui.add_space(2.0);
                render_side_pane(ui, "ancestor", anc, bg_ancestor());
            }

            // ----- Optional custom-editor -----
            if *open_custom {
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Custom resolution (will replace the hunk)")
                        .small()
                        .strong(),
                );
                // We seed the textarea from `state.custom_text`; the
                // caller wrote the seed in when `OpenCustom` fired on the
                // previous frame. We edit a local buffer to avoid holding
                // a &mut into `state`.
                let mut buf = state.custom_text.clone();
                let response = ui.add(
                    TextEdit::multiline(&mut buf)
                        .desired_rows(6)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace),
                );
                if response.changed() {
                    intents.push(HunkIntent::EditCustom {
                        index: idx,
                        text: buf,
                    });
                }
            }

            // ----- Preview of what lands in the final file -----
            ui.add_space(4.0);
            ui.label(
                RichText::new("Preview — what this hunk becomes in the saved file:")
                    .small()
                    .weak(),
            );
            let preview = preview_text(chunk, state);
            let preview_display = if preview.is_empty() {
                "(empty — this hunk will be dropped)".to_string()
            } else {
                preview.strip_suffix('\n').unwrap_or(&preview).to_string()
            };
            egui::Frame::none()
                .fill(Color32::from_rgba_unmultiplied(90, 190, 140, 30))
                .inner_margin(egui::Margin::symmetric(6.0, 3.0))
                .show(ui, |ui| {
                    ui.style_mut().override_text_style =
                        Some(egui::TextStyle::Monospace);
                    ui.label(preview_display);
                });
        });
}

/// Compute what a hunk's resolved content will look like. Used for the
/// inline preview under each card. We intentionally don't call
/// `compose_resolved_text` here because that returns `None` on any
/// pending hunk — we want a per-hunk preview even for pending ones (the
/// preview shows "(nothing yet)" so the user understands the blank
/// state).
fn preview_text(chunk: &ConflictHunkKind, state: &HunkResolutionState) -> String {
    let ConflictHunkKind::Conflict { ours, theirs, .. } = chunk else {
        return String::new();
    };
    match state.resolution {
        HunkResolution::Pending => String::new(),
        HunkResolution::Ours => ours.clone(),
        HunkResolution::Theirs => theirs.clone(),
        HunkResolution::Both => {
            let mut buf = ours.clone();
            if !ours.ends_with('\n') && !ours.is_empty() {
                buf.push('\n');
            }
            buf.push_str(theirs);
            buf
        }
        HunkResolution::Custom => state.custom_text.clone(),
    }
}

/// Tinted, monospace-rendered display of one side of a conflict.
fn render_side_pane(ui: &mut egui::Ui, label: &str, text: &str, bg: Color32) {
    egui::Frame::none()
        .fill(bg)
        .inner_margin(egui::Margin::symmetric(6.0, 4.0))
        .rounding(egui::Rounding::same(3.0))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(label).small().strong());
                ui.style_mut().override_text_style = Some(egui::TextStyle::Monospace);
                let display = if text.is_empty() {
                    "(empty)".to_string()
                } else {
                    text.strip_suffix('\n').unwrap_or(text).to_string()
                };
                ui.label(display);
            });
        });
}

/// Coloured "Pending / Ours / Theirs / Both / Custom" pill used in each
/// card's header. Amber for Pending to match the warning banner colour
/// from `src/ui/conflicts.rs::palette::WARNING`.
fn status_pill(ui: &mut egui::Ui, res: HunkResolution) {
    let (text, color) = match res {
        HunkResolution::Pending => ("Pending", PILL_PENDING),
        HunkResolution::Ours => ("Ours", PILL_OURS),
        HunkResolution::Theirs => ("Theirs", PILL_THEIRS),
        HunkResolution::Both => ("Both", PILL_BOTH),
        HunkResolution::Custom => ("Custom", PILL_CUSTOM),
    };
    ui.colored_label(
        color,
        RichText::new(format!("● {text}")).small().strong(),
    );
}

// ---------------------------------------------------------------------------
// Keyboard handling
// ---------------------------------------------------------------------------

/// Keyboard shortcuts for the hunk editor. Invoked after `render` so any
/// pressed keys this frame are reflected in the next frame's render.
///
/// Shortcuts:
///   * `1` — current hunk = Ours
///   * `2` — current hunk = Theirs
///   * `3` — current hunk = Both
///   * `n` — jump to next Pending hunk
///   * `p` — jump to previous Pending hunk
///
/// We refuse to fire any shortcut when a text input has keyboard focus —
/// otherwise typing "3" into a custom textarea would silently change the
/// resolution. egui exposes this through `ctx.memory(|m| m.focused())`.
pub fn handle_shortcuts(
    ctx: &egui::Context,
    has_conflicts: bool,
    intents: &mut Vec<HunkIntent>,
) {
    if !has_conflicts {
        return;
    }
    // If any widget currently has focus, we assume the user is typing
    // into a TextEdit (the only focusable thing in this window besides
    // buttons) and silently skip shortcut handling for this frame.
    let text_focus = ctx.memory(|m| m.focused().is_some());
    if text_focus {
        return;
    }
    ctx.input(|i| {
        if i.key_pressed(egui::Key::Num1) {
            intents.push(HunkIntent::SetActive {
                resolution: HunkResolution::Ours,
            });
        }
        if i.key_pressed(egui::Key::Num2) {
            intents.push(HunkIntent::SetActive {
                resolution: HunkResolution::Theirs,
            });
        }
        if i.key_pressed(egui::Key::Num3) {
            intents.push(HunkIntent::SetActive {
                resolution: HunkResolution::Both,
            });
        }
        if i.key_pressed(egui::Key::N) {
            intents.push(HunkIntent::NavPending { forward: true });
        }
        if i.key_pressed(egui::Key::P) {
            intents.push(HunkIntent::NavPending { forward: false });
        }
    });
}

/// Move `active_hunk` to the next `Pending` hunk (or previous if
/// `forward == false`). Returns the new index (or `None` if nothing is
/// pending, in which case the caller should keep the current cursor).
///
/// Wraps around — reaching the end goes back to the first pending hunk.
/// That's the right behaviour in practice: users hit `n` to "find the
/// next thing to do", and wrapping means they don't have to remember
/// which direction they were scanning.
pub fn next_pending(
    resolutions: &[HunkResolutionState],
    from: Option<usize>,
    forward: bool,
) -> Option<usize> {
    let n = resolutions.len();
    if n == 0 {
        return None;
    }
    let start = from.unwrap_or(0);
    // Walk up to `n` positions (full circle) looking for pending.
    for step in 1..=n {
        let idx = if forward {
            (start + step) % n
        } else {
            (start + n - step) % n
        };
        if resolutions[idx].resolution == HunkResolution::Pending {
            return Some(idx);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn st(res: HunkResolution) -> HunkResolutionState {
        HunkResolutionState {
            resolution: res,
            custom_text: String::new(),
        }
    }

    #[test]
    fn next_pending_wraps_forward() {
        let r = vec![
            st(HunkResolution::Ours),
            st(HunkResolution::Pending),
            st(HunkResolution::Theirs),
        ];
        assert_eq!(next_pending(&r, Some(0), true), Some(1));
        // From the only pending hunk → wrap to itself? No — "next" should
        // walk past the current one. With only one pending, wrapping
        // lands back on it, which is the right answer.
        assert_eq!(next_pending(&r, Some(1), true), Some(1));
    }

    #[test]
    fn next_pending_returns_none_when_all_resolved() {
        let r = vec![st(HunkResolution::Ours), st(HunkResolution::Theirs)];
        assert_eq!(next_pending(&r, Some(0), true), None);
        assert_eq!(next_pending(&r, Some(0), false), None);
    }

    #[test]
    fn next_pending_walks_backward() {
        let r = vec![
            st(HunkResolution::Pending),
            st(HunkResolution::Ours),
            st(HunkResolution::Pending),
        ];
        assert_eq!(next_pending(&r, Some(2), false), Some(0));
        assert_eq!(next_pending(&r, Some(0), false), Some(2));
    }

    #[test]
    fn preview_text_pending_is_empty() {
        let chunk = ConflictHunkKind::Conflict {
            ours: "O\n".into(),
            theirs: "T\n".into(),
            ancestor: None,
        };
        assert!(preview_text(&chunk, &st(HunkResolution::Pending)).is_empty());
    }

    #[test]
    fn preview_text_custom_uses_state() {
        let chunk = ConflictHunkKind::Conflict {
            ours: "O\n".into(),
            theirs: "T\n".into(),
            ancestor: None,
        };
        let state = HunkResolutionState {
            resolution: HunkResolution::Custom,
            custom_text: "X\n".into(),
        };
        assert_eq!(preview_text(&chunk, &state), "X\n");
    }

    #[test]
    fn preview_text_both_joins_with_newline() {
        let chunk = ConflictHunkKind::Conflict {
            ours: "O".into(),
            theirs: "T\n".into(),
            ancestor: None,
        };
        assert_eq!(preview_text(&chunk, &st(HunkResolution::Both)), "O\nT\n");
    }
}
