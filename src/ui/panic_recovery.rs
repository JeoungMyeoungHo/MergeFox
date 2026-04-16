//! Panic Recovery modal — surfaces when the user has been rapidly
//! undoing/redoing and may have lost track of where they are.
//!
//! Offers three paths back to a "known good" state:
//!   * last clean (non-WIP) commit entry on the current branch
//!   * last entry before the current panic burst
//!   * full timeline viewer (history panel — TBD)

use egui::{Color32, RichText};

use crate::app::{MergeFoxApp, View};

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    if !app.panic_modal_open {
        return;
    }

    // Snapshot the info we need first — avoids holding a borrow through the modal.
    let options = collect_options(app);
    let mut open = true;
    let mut pick: Option<u64> = None;
    let mut close = false;

    egui::Window::new("🆘 Recovery")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(520.0)
        .show(ctx, |ui| {
            ui.label("You've been undoing and redoing a lot.");
            ui.label("Where do you want to return to?");
            ui.add_space(8.0);

            if options.is_empty() {
                ui.weak("No recoverable states found in journal.");
            }

            for opt in &options {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&opt.icon).size(18.0));
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&opt.title).strong());
                        ui.weak(&opt.detail);
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Restore").clicked() {
                            pick = Some(opt.entry_id);
                        }
                    });
                });
                ui.separator();
            }

            ui.horizontal(|ui| {
                if ui.button("Close").clicked() {
                    close = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak("Esc to close");
                });
            });
        });

    if let Some(id) = pick {
        app.restore_to_entry(id);
    }
    if close || !open {
        app.panic_modal_open = false;
    }

    // Esc also closes.
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.panic_modal_open = false;
    }
}

struct Option_ {
    icon: String,
    title: String,
    detail: String,
    entry_id: u64,
}

fn collect_options(app: &MergeFoxApp) -> Vec<Option_> {
    let mut out = Vec::new();
    let View::Workspace(tabs) = &app.view else {
        return out;
    };
    let ws = tabs.current();
    let Some(journal) = ws.journal.as_ref() else {
        return out;
    };

    // Option 1: last entry whose BEFORE state had a clean working tree
    //           and represented a commit landing (Commit | Merge).
    if let Some(entry) = journal
        .entries
        .iter()
        .rev()
        .find(|e| !e.before.working_dirty && is_landing_op(&e.operation))
    {
        out.push(Option_ {
            icon: "⭐".into(),
            title: format!("Last clean commit ({})", entry.operation.label()),
            detail: format!(
                "On {} · {}",
                entry.before.head_branch.as_deref().unwrap_or("(detached)"),
                relative(entry.timestamp_unix)
            ),
            entry_id: entry.id,
        });
    }

    // Option 2: earliest entry in the current panic window.
    if app.nav_history.len() >= 2 {
        // Find the oldest journal entry whose id is less than the current cursor.
        if let Some(cursor_idx) = journal.cursor {
            // Walk back up to ~10 entries and pick the first non-navigational one.
            let start = cursor_idx.saturating_sub(10);
            if let Some(entry) = journal.entries[start..=cursor_idx]
                .iter()
                .find(|e| !is_nav_op(&e.operation))
            {
                out.push(Option_ {
                    icon: "🏠".into(),
                    title: "Before this burst".into(),
                    detail: format!(
                        "Jump back past ~{} recent operations",
                        cursor_idx - start + 1
                    ),
                    entry_id: entry.id,
                });
            }
        }
    }

    // Option 3: full timeline (not yet implemented — placeholder).
    // Hidden until history panel exists.
    let _ = Color32::YELLOW;

    out
}

fn is_landing_op(op: &crate::journal::Operation) -> bool {
    use crate::journal::Operation::*;
    matches!(op, Commit { .. } | Merge { .. } | CherryPick { .. })
}

fn is_nav_op(_op: &crate::journal::Operation) -> bool {
    // Currently we don't model Undo/Redo as their own ops — they just move
    // the cursor. Placeholder for when we do.
    false
}

fn relative(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let diff = (now - ts).max(0);
    match diff {
        d if d < 60 => "moments ago".into(),
        d if d < 3600 => format!("{}m ago", d / 60),
        d if d < 86_400 => format!("{}h ago", d / 3600),
        d => format!("{}d ago", d / 86_400),
    }
}
