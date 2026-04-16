use std::time::{SystemTime, UNIX_EPOCH};

use egui::RichText;

use crate::app::{MergeFoxApp, View};
use crate::journal::{self, Operation};

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    if !app.reflog_open {
        return;
    }

    let entries = {
        let View::Workspace(tabs) = &app.view else {
            app.reflog_open = false;
            return;
        };
        let ws = tabs.current();
        match ws.repo.head_reflog(40) {
            Ok(entries) => entries,
            Err(err) => {
                app.last_error = Some(format!("reflog: {err:#}"));
                app.reflog_open = false;
                return;
            }
        }
    };

    let mut open = true;
    let mut restore_oid: Option<gix::ObjectId> = None;

    egui::Window::new("Reflog Recovery")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(760.0)
        .default_height(560.0)
        .show(ctx, |ui| {
            ui.label("Recover earlier HEAD positions without moving the current branch destructively.");
            ui.weak("Restore creates a new local recovery branch at the chosen reflog entry, then checks it out.");
            ui.separator();

            if entries.is_empty() {
                ui.weak("No reflog entries available for HEAD.");
                return;
            }

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for entry in &entries {
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!("#{}", entry.index))
                                        .monospace()
                                        .weak(),
                                );
                                ui.label(
                                    RichText::new(primary_message(&entry.message)).strong(),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.button("Restore").clicked() {
                                            restore_oid = Some(entry.new_oid);
                                        }
                                    },
                                );
                            });
                            ui.horizontal_wrapped(|ui| {
                                ui.weak(format!("{} → {}", short_sha(&entry.old_oid), short_sha(&entry.new_oid)));
                                if !entry.committer.is_empty() {
                                    ui.weak("·");
                                    ui.weak(&entry.committer);
                                }
                                ui.weak("·");
                                ui.weak(relative_time(entry.timestamp));
                            });
                            if !entry.message.trim().is_empty() {
                                ui.weak(entry.message.trim());
                            }
                        });
                        ui.add_space(4.0);
                    }
                });
        });

    if let Some(oid) = restore_oid {
        restore_reflog_entry(app, oid);
    }
    if !open {
        app.reflog_open = false;
    }
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.reflog_open = false;
    }
}

fn restore_reflog_entry(app: &mut MergeFoxApp, oid: gix::ObjectId) {
    let mut hud = None;
    let mut error = None;
    let mut rebuild = None;
    let mut journal_entry = None;

    {
        let View::Workspace(tabs) = &mut app.view else {
            return;
        };
        let ws = tabs.current_mut();
        let before = journal::capture(ws.repo.path()).ok();
        let outcome = (|| -> anyhow::Result<String> {
            let branch = ws.repo.create_recovery_branch(oid)?;
            ws.repo.checkout_branch(&branch)?;
            Ok(branch)
        })();

        match outcome {
            Ok(branch) => {
                if let (Some(before), Ok(after)) = (before, journal::capture(ws.repo.path())) {
                    journal_entry = Some((
                        Operation::Raw {
                            label: format!("Restore reflog {branch}"),
                        },
                        before,
                        after,
                    ));
                }
                hud = Some(format!(
                    "Checked out recovery branch {branch} at {}",
                    short_sha(&oid)
                ));
                rebuild = Some(ws.graph_scope);
            }
            Err(err) => {
                error = Some(format!("restore reflog entry: {err:#}"));
            }
        }
    }

    if let Some((op, before, after)) = journal_entry {
        app.journal_record(op, before, after);
    }
    if let Some(scope) = rebuild {
        app.rebuild_graph(scope);
    }
    if let Some(hud) = hud {
        app.hud = Some(crate::app::Hud::new(hud, 2200));
    }
    if let Some(error) = error {
        app.last_error = Some(error);
    }
}

fn primary_message(message: &str) -> String {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        "(no reflog message)".to_string()
    } else {
        trimmed.lines().next().unwrap_or(trimmed).to_string()
    }
}

fn relative_time(ts: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
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

fn short_sha(oid: &gix::ObjectId) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}
