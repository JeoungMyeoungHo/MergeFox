//! Reword-any-commit modal.
//!
//! Non-HEAD commit messages are a pain to edit from stock git — you
//! have to remember `git rebase -i <oid>^`, mark the target as
//! `reword`, survive the editor dance, and hope the descendants
//! rebase cleanly. This modal is the GUI wrapper: the user edits a
//! text field, checks (or leaves on) "Create backup tag", presses
//! Reword, and the whole flow runs on a worker thread behind the
//! usual basket-style backup + auto-stash envelope.
//!
//! The actual git work lives in `git::reword_commit`. This file is
//! strictly presentation + intent routing.

use egui::{Color32, RichText, TextEdit};

use crate::app::{MergeFoxApp, View};

/// State the modal carries between frames. Kept on `MergeFoxApp`
/// (like `basket_squash_confirm`) so tab switches don't lose it.
#[derive(Debug, Clone)]
pub struct RewordModalState {
    pub target_oid: gix::ObjectId,
    pub short_oid: String,
    pub original_message: String,
    pub edited_message: String,
    pub upstream_warning: Option<String>,
    pub head_warning: bool,
    pub busy: bool,
}

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    let Some(state) = app.reword_modal.clone() else {
        return;
    };

    let mut open = true;
    let mut confirm = false;
    let mut cancel = false;
    let mut edited = state.edited_message.clone();

    egui::Window::new(format!("Edit commit message — {}", state.short_oid))
        .id(egui::Id::new("mergefox-reword-modal"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(640.0)
        .default_height(360.0)
        .show(ctx, |ui| {
            ui.label(
                RichText::new(format!("Commit {}", state.short_oid))
                    .monospace()
                    .weak(),
            );

            // Warnings get their own prominent lines above the editor
            // so users can't miss them. These are the two risks that
            // matter for a reword: history rewrite on published work
            // (force-push needed) and accidental HEAD-vs-non-HEAD
            // confusion.
            if let Some(w) = &state.upstream_warning {
                ui.add_space(6.0);
                ui.colored_label(Color32::from_rgb(240, 180, 96), format!("⚠ {w}"));
            }
            if state.head_warning {
                ui.colored_label(
                    Color32::from_gray(170),
                    "HEAD commit — this uses `git commit --amend`.",
                );
            } else {
                ui.colored_label(
                    Color32::from_gray(170),
                    "Non-HEAD commit — a backup tag is created and descendants are rebased.",
                );
            }

            ui.add_space(8.0);
            ui.label(RichText::new("New message").strong());
            ui.add(
                TextEdit::multiline(&mut edited)
                    .desired_rows(12)
                    .desired_width(f32::INFINITY)
                    .font(egui::TextStyle::Monospace),
            );

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let can_confirm = !state.busy
                            && !edited.trim().is_empty()
                            && edited != state.original_message;
                        let label = if state.busy {
                            "Rewording…"
                        } else {
                            "Reword"
                        };
                        let resp = ui.add_enabled(
                            can_confirm,
                            egui::Button::new(
                                RichText::new(label)
                                    .color(Color32::WHITE)
                                    .strong(),
                            )
                            .fill(Color32::from_rgb(86, 156, 214)),
                        );
                        if resp.clicked() {
                            confirm = true;
                        }
                        if state.busy {
                            ui.add_space(6.0);
                            ui.spinner();
                        }
                    },
                );
            });

            ui.add_space(4.0);
            ui.weak(
                "Original message preserved below for reference. \
                 Author / committer identity + dates are kept unchanged.",
            );
            ui.group(|ui| {
                ui.set_max_height(120.0);
                egui::ScrollArea::vertical()
                    .id_salt("reword-original-preview")
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(&state.original_message)
                                    .monospace()
                                    .weak(),
                            )
                            .wrap(),
                        );
                    });
            });
        });

    // Persist any typing the user did.
    if let Some(state) = app.reword_modal.as_mut() {
        state.edited_message = edited;
    }

    if cancel || !open {
        app.reword_modal = None;
    } else if confirm {
        app.start_reword();
    }
}
