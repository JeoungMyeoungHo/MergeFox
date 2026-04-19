//! Confirmation modal for the basket "Squash into one" operation.
//!
//! The squash rewrites history in-place (moves the current branch ref
//! to a brand-new commit that replaces the selected N commits) and
//! then rebases the intervening commits on top. That's the single most
//! destructive thing a basket can do, so we gate the action behind an
//! explicit confirmation with:
//!
//!   * A loud warning that this will need a force push if the branch
//!     was already pushed.
//!   * A named backup tag we'll create before touching anything.
//!   * An editable commit-message textarea pre-filled with the
//!     composed summaries of the selected commits (see
//!     `basket_ops::compose_default_squash_message`).
//!
//! The modal is intentionally modal-blocking (no background click
//! escape). We render with `egui::Window` anchored centre-top so it
//! lines up with the basket bar; the caller (app.rs) keeps the
//! `BasketSquashConfirmState` in `Option` form on `MergeFoxApp` so
//! `None` = closed, `Some(..)` = open.

use egui::{Align2, Color32, Key, RichText};

/// Persistent state for the confirm dialog. Lives on `MergeFoxApp`.
///
/// `message` starts as the default composition of basket commit
/// summaries; the user can edit it freely. `backup_tag_preview` is the
/// tag name we *would* create — rendered in the dialog so the user
/// knows what to look for in `git tag -l 'mergefox/*'` afterwards. We
/// regenerate it if the modal reopens, so it always reflects "now".
#[derive(Debug, Clone)]
pub struct BasketSquashConfirmState {
    pub message: String,
    pub backup_tag_preview: String,
    /// How many commits are selected at open time — shown in the
    /// header so the user doesn't have to count the basket bar.
    pub commit_count: usize,
}

/// The user's choice this frame.
#[derive(Debug, Clone)]
pub enum BasketSquashConfirmOutcome {
    /// Go ahead — start the worker with this message.
    Confirmed { message: String },
    /// Dismiss without side effects.
    Cancelled,
}

/// Render the confirm modal. Returns an outcome once the user clicks
/// Cancel / Squash or presses Esc / Ctrl+Enter. The caller owns the
/// state lifecycle: clear it after a non-None return.
///
/// WHY separate `state.message` (mutable) from the outcome (by-value
/// String): the textbox writes into `state.message` every frame; we
/// hand a `clone()` to the caller on confirm so the caller can drop the
/// state immediately without worrying about borrow lifetimes.
pub fn show(
    ctx: &egui::Context,
    state: &mut BasketSquashConfirmState,
) -> Option<BasketSquashConfirmOutcome> {
    let mut outcome: Option<BasketSquashConfirmOutcome> = None;

    egui::Window::new("Squash basket into one commit")
        .title_bar(true)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, [0.0, 96.0])
        .fixed_size([560.0, 0.0])
        .show(ctx, |ui| {
            // ---- Warning banner ----
            // A yellow stripe is more arresting than a plain label;
            // users skim past plain text in modal dialogs. The warning
            // copy names both consequences (force push) and the
            // mitigation (backup tag) in the same block.
            let warn_bg = Color32::from_rgb(80, 60, 10);
            egui::Frame::none()
                .fill(warn_bg)
                .rounding(4.0)
                .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(format!(
                            "⚠ This rewrites history for {} commits.",
                            state.commit_count
                        ))
                        .strong()
                        .color(Color32::from_rgb(255, 220, 120)),
                    );
                    ui.label(
                        RichText::new(
                            "Existing branches or remotes will need a force push \
                             after this operation. Anyone else working from the \
                             same branch will have to re-sync.",
                        )
                        .color(Color32::from_rgb(240, 230, 210)),
                    );
                });

            ui.add_space(10.0);

            // ---- Backup tag preview ----
            ui.label(RichText::new("Safety net").strong());
            ui.horizontal_wrapped(|ui| {
                ui.label("Before squashing, we'll create a backup tag at the current HEAD:");
            });
            ui.add_space(2.0);
            ui.code(&state.backup_tag_preview);
            ui.weak(
                "You can restore the old history any time with \
                 `git reset --hard <that tag>`.",
            );

            ui.add_space(12.0);

            // ---- Commit message editor ----
            ui.label(RichText::new("Commit message").strong());
            ui.weak(
                "Prefilled from the basket's commit summaries — edit freely.",
            );
            ui.add_space(4.0);

            let textedit = egui::TextEdit::multiline(&mut state.message)
                .desired_rows(8)
                .desired_width(f32::INFINITY)
                .hint_text("Squash of N commits\n\n* first\n* second\n…");
            ui.add(textedit);

            ui.add_space(10.0);

            // ---- Action row ----
            // Right-aligned so the primary action ("Squash") is under
            // the user's mouse after reading the warning / message —
            // matches the rest of the app's destructive-action dialogs.
            ui.horizontal(|ui| {
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let can_confirm = !state.message.trim().is_empty();
                        let squash_btn = egui::Button::new(
                            RichText::new("⇩  Squash")
                                .color(Color32::from_rgb(255, 240, 230)),
                        )
                        .fill(Color32::from_rgb(160, 70, 40));
                        if ui.add_enabled(can_confirm, squash_btn).clicked() {
                            outcome = Some(BasketSquashConfirmOutcome::Confirmed {
                                message: state.message.clone(),
                            });
                        }
                        if ui.button("Cancel").clicked() {
                            outcome = Some(BasketSquashConfirmOutcome::Cancelled);
                        }
                    },
                );
            });
        });

    // Keyboard: Esc cancels. Ctrl/Cmd+Enter confirms (same chord as
    // the commit modal) — faster than mouse for the power user.
    if ctx.input(|i| i.key_pressed(Key::Escape)) {
        outcome = Some(BasketSquashConfirmOutcome::Cancelled);
    }
    if ctx.input(|i| {
        (i.modifiers.ctrl || i.modifiers.mac_cmd) && i.key_pressed(Key::Enter)
    }) && !state.message.trim().is_empty()
    {
        outcome = Some(BasketSquashConfirmOutcome::Confirmed {
            message: state.message.clone(),
        });
    }

    outcome
}
