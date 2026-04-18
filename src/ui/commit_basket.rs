//! Floating action bar for the multi-commit basket.
//!
//! Surfaces cross-cutting operations that are hard to reach from stock
//! Git CLI:
//!   * See the combined diff of N cherry-picked commits without applying
//!     anything to the working tree.
//!   * Filter that combined diff to a single file path — "what did these
//!     four commits do to `foo.tsx`?"
//!   * Revert the combined set onto the working tree (for the "undo
//!     these N non-linear commits in one go" flow).
//!   * Squash the selection into one commit, even when they aren't
//!     contiguous in history. (Gated behind a confirm dialog — rewrites
//!     history.)
//!
//! The bar is an `egui::Area` anchored `CENTER_BOTTOM` so it floats over
//! the workspace without reserving layout space. Only rendered when
//! `ws.commit_basket.len() >= 2` — a single-commit basket is redundant
//! with the existing single-select flow.
//!
//! Intents are routed back via `BasketIntent` rather than calling into
//! the app directly; the caller (main_panel) dispatches after the UI
//! closure drops its `&mut WorkspaceState` borrow.

use egui::{Align2, Color32, RichText, Stroke};

use crate::app::WorkspaceState;

/// What the user clicked in the basket bar this frame. At most one
/// intent per frame — the bar buttons are mutually exclusive.
#[derive(Clone, Debug)]
pub enum BasketIntent {
    /// Read-only view: open the combined diff of all basket commits.
    ShowCombinedDiff,
    /// Ask the user to pick a file, then show the combined diff
    /// filtered to that file.
    FocusFile,
    /// `git revert --no-commit <oid1> <oid2> ...`. Dirty working tree
    /// is auto-stashed first.
    RevertToWorkingTree,
    /// Non-linear squash: cherry-pick all into one new commit, rebase
    /// HEAD on top. Highest risk — caller should show a confirmation.
    SquashIntoOne,
    /// Empty the basket.
    Clear,
}

/// Render the basket bar. Returns the user's intent this frame (if any)
/// so the caller can act after the UI closure releases `ws`.
pub fn show(ctx: &egui::Context, ws: &WorkspaceState) -> Option<BasketIntent> {
    if ws.commit_basket.len() < 2 {
        return None;
    }

    let mut intent: Option<BasketIntent> = None;
    let count = ws.commit_basket.len();

    egui::Area::new(egui::Id::new("mergefox-commit-basket"))
        .order(egui::Order::Foreground)
        .anchor(Align2::CENTER_BOTTOM, [0.0, -16.0])
        .show(ctx, |ui| {
            let accent = ui.visuals().selection.bg_fill;
            let bg = ui.visuals().window_fill();
            egui::Frame::window(ui.style())
                .fill(bg)
                .stroke(Stroke::new(1.0, accent))
                .rounding(8.0)
                .inner_margin(egui::Margin::symmetric(14.0, 10.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("✓ {count} commits selected"))
                                .color(accent_text_color(accent))
                                .strong(),
                        );
                        ui.add_space(6.0);
                        ui.label(RichText::new("•").color(Color32::from_gray(120)));
                        ui.add_space(6.0);

                        if ui
                            .button("✦ Combined diff")
                            .on_hover_text(
                                "View the cumulative diff of these commits as if they were \
                                 a single commit — no working-tree changes.",
                            )
                            .clicked()
                        {
                            intent = Some(BasketIntent::ShowCombinedDiff);
                        }

                        if ui
                            .button("🔍 Focus file…")
                            .on_hover_text(
                                "Pick a file and show only that file's combined diff \
                                 across these commits.",
                            )
                            .clicked()
                        {
                            intent = Some(BasketIntent::FocusFile);
                        }

                        if ui
                            .button("↺ Revert to WT")
                            .on_hover_text(
                                "Apply the reverse of these commits to the working tree \
                                 (git revert --no-commit). Dirty WT is auto-stashed first.",
                            )
                            .clicked()
                        {
                            intent = Some(BasketIntent::RevertToWorkingTree);
                        }

                        if ui
                            .button("⇩ Squash into one")
                            .on_hover_text(
                                "Rewrite history: combine these non-linear commits into a \
                                 single commit and rebase the rest of the branch on top. \
                                 Creates a backup tag.",
                            )
                            .clicked()
                        {
                            intent = Some(BasketIntent::SquashIntoOne);
                        }

                        ui.add_space(6.0);
                        ui.label(RichText::new("•").color(Color32::from_gray(120)));
                        ui.add_space(6.0);

                        if ui
                            .button("✕ Clear")
                            .on_hover_text("Empty the basket.")
                            .clicked()
                        {
                            intent = Some(BasketIntent::Clear);
                        }
                    });
                });
        });

    intent
}

/// The accent fill is dark on dark themes and bright on light — the
/// selection colour from `Visuals` isn't guaranteed to have enough
/// contrast against itself as text. Pick a reading colour based on
/// luminance so the "N commits selected" label is always readable.
fn accent_text_color(accent: Color32) -> Color32 {
    // Rough perceptual luminance. Exact Rec.709 coefficients; we only
    // need "is this bright-ish?" resolution.
    let r = accent.r() as f32;
    let g = accent.g() as f32;
    let b = accent.b() as f32;
    let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    if luma > 140.0 {
        Color32::from_rgb(30, 30, 30)
    } else {
        Color32::from_rgb(235, 235, 240)
    }
}
