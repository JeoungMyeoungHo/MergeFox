//! Keyboard shortcut cheat-sheet modal.
//!
//! Opened with `?` (or `Shift+/` on most keyboards). The full list of
//! bindings lives here rather than being scattered across the UI so
//! there is one canonical source to skim when a user asks "what can I
//! press?". Each row is `(keys, description)`.
//!
//! Any new global hotkey added to `app::handle_hotkeys` should add a
//! row here too; keeping them in sync is how we fulfil the "document
//! your shortcuts" part of C9.

use egui::{Align2, RichText};

use crate::app::MergeFoxApp;

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    if !app.shortcuts_open {
        return;
    }

    let mut open = true;
    egui::Window::new("Keyboard shortcuts")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .default_width(420.0)
        .show(ctx, |ui| {
            ui.add_space(4.0);
            section(ui, "Navigation");
            row(ui, "Cmd/Ctrl+K", "Open the command palette");
            row(ui, "Ctrl+Tab", "Next workspace tab");
            row(ui, "Ctrl+Shift+Tab", "Previous workspace tab");
            row(ui, "Cmd/Ctrl+W", "Close the active tab");

            ui.add_space(8.0);
            section(ui, "History / safety");
            row(ui, "Cmd/Ctrl+Z", "Undo last mutating action");
            row(ui, "Cmd/Ctrl+Shift+Z", "Redo");
            row(ui, "Cmd/Ctrl+Shift+R", "Open reflog recovery");
            row(ui, "Cmd/Ctrl+Shift+Esc", "Panic-recovery modal");

            ui.add_space(8.0);
            section(ui, "This dialog");
            row(ui, "?", "Show this cheat-sheet");
            row(ui, "Esc", "Close the top-most modal");

            ui.add_space(8.0);
            ui.weak(
                "Shortcuts that conflict with a focused text field (typing `?` \
                 into a commit message, for example) are suppressed — the \
                 field wins. Release focus with Esc first.",
            );
        });

    if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.shortcuts_open = false;
    }
}

fn section(ui: &mut egui::Ui, label: &str) {
    ui.label(RichText::new(label).strong());
    ui.separator();
}

fn row(ui: &mut egui::Ui, keys: &str, desc: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(
            egui::vec2(150.0, 18.0),
            egui::Label::new(RichText::new(keys).monospace()),
        );
        ui.label(desc);
    });
}
