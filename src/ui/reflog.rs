//! Reflog recovery window — two actions per reflog entry:
//!
//!   * **Restore** (safe, default) — creates a fresh `recovery/<short>`
//!     branch at the entry's oid and checks it out. Leaves the current
//!     branch ref untouched, so the user can always `git switch -` back
//!     to where they were. Existing behaviour; unchanged.
//!   * **Reset to here** (destructive) — moves the current branch ref
//!     back to the entry with `git reset --hard`. Any commits on top of
//!     the previous HEAD become unreachable. Wrapped in a confirmation
//!     modal that previews the lost commits and enforces an "I
//!     understand" checkbox before enabling the destructive button.
//!
//! We also render a **Quick jump** strip at the top of the window with
//! up to three pills — "5 minutes ago", "1 hour ago", "yesterday" —
//! each pre-filling the Reset modal with the most-recent reflog entry
//! at least that old. The pills make the common "I just messed up, put
//! me back where I was five minutes ago" action a single click away
//! without scrolling.
//!
//! The reflog panel itself owns no destructive state: clicking a pill
//! or the red "Reset to here" button only calls
//! `MergeFoxApp::show_reflog_rewind_confirm`, which opens the modal.
//! The modal renders in `reflog_rewind_confirm_modal` at the bottom of
//! this file; the actual reset runs on a worker thread, so a long
//! reset doesn't freeze the UI.

use std::time::{SystemTime, UNIX_EPOCH};

use egui::{Color32, RichText};

use crate::app::{MergeFoxApp, View};
use crate::journal::{self, Operation};

/// Destructive-action accent. Intentionally darker than egui's default
/// red so it reads as "danger" against our theme rather than "error".
/// Same hex the task brief calls out; kept here as a const so future UI
/// changes can re-use it.
const DESTRUCTIVE_RED: Color32 = Color32::from_rgb(212, 92, 92);

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    // The confirmation modal is driven off `reflog_rewind_confirm`;
    // render it regardless of whether the underlying reflog panel is
    // currently open. WHY: the user can dismiss the reflog window
    // while the modal is up (we close the reflog on successful reset
    // later) — the modal must not disappear just because its parent
    // did.
    reflog_rewind_confirm_modal(ctx, app);

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
    // We collect click actions into this enum and apply them AFTER the
    // window scope closes, so mutably borrowing `app` inside the
    // closure doesn't clash with the `app` we pass to
    // `show_reflog_rewind_confirm` / `restore_reflog_entry`.
    let mut action: Option<ReflogAction> = None;

    let quick_jumps = crate::git::pick_quick_jumps(&entries);

    egui::Window::new("Reflog Recovery")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(780.0)
        .default_height(580.0)
        .show(ctx, |ui| {
            ui.label("Recover earlier HEAD positions. Two paths: safe (Restore creates a recovery branch) and destructive (Reset moves the current branch ref back).");
            ui.weak("Restore is always reversible. Reset rewrites history — a backup tag is always created first so you can roll back.");
            ui.separator();

            if !quick_jumps.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.strong("Jump to:");
                    for target in &quick_jumps {
                        let btn = egui::Button::new(
                            RichText::new(&target.label).small(),
                        )
                        .fill(ui.visuals().faint_bg_color)
                        .stroke(egui::Stroke::new(1.0, DESTRUCTIVE_RED));
                        let resp = ui.add(btn).on_hover_text(format!(
                            "Open the Reset confirmation for the reflog entry at {} ({}).",
                            short_sha(&target.oid),
                            target.message.trim()
                        ));
                        if resp.clicked() {
                            action = Some(ReflogAction::OpenRewindConfirm {
                                oid: target.oid,
                                message: Some(target.message.clone()),
                            });
                        }
                    }
                });
                ui.separator();
            }

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
                                        // Destructive button on the OUTSIDE (rightmost),
                                        // safe button on the inside. Reading right-to-left
                                        // (because of the layout) gives "Reset · Restore" —
                                        // destructive first, matching the task brief's
                                        // hierarchy call.
                                        let reset_btn = egui::Button::new(
                                            RichText::new("Reset to here")
                                                .color(Color32::WHITE),
                                        )
                                        .fill(DESTRUCTIVE_RED);
                                        let resp = ui.add(reset_btn).on_hover_text(
                                            "Move the current branch ref to this entry with \
                                             git reset --hard. A backup tag is created first \
                                             so the old HEAD stays recoverable.",
                                        );
                                        if resp.clicked() {
                                            action = Some(ReflogAction::OpenRewindConfirm {
                                                oid: entry.new_oid,
                                                message: Some(entry.message.clone()),
                                            });
                                        }
                                        if ui
                                            .button("Restore")
                                            .on_hover_text(
                                                "Safe: create a new local recovery branch at \
                                                 this entry and check it out. Leaves the \
                                                 current branch ref untouched.",
                                            )
                                            .clicked()
                                        {
                                            action = Some(ReflogAction::Restore {
                                                oid: entry.new_oid,
                                            });
                                        }
                                    },
                                );
                            });
                            ui.horizontal_wrapped(|ui| {
                                ui.weak(format!(
                                    "{} → {}",
                                    short_sha(&entry.old_oid),
                                    short_sha(&entry.new_oid)
                                ));
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

    if let Some(act) = action {
        match act {
            ReflogAction::Restore { oid } => restore_reflog_entry(app, oid),
            ReflogAction::OpenRewindConfirm { oid, message } => {
                app.show_reflog_rewind_confirm(oid, message);
            }
        }
    }
    if !open {
        app.reflog_open = false;
    }
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        // Don't close the reflog while the confirm modal is visible —
        // escape should dismiss the modal first, which its own handler
        // takes care of.
        if app.reflog_rewind_confirm.is_none() {
            app.reflog_open = false;
        }
    }
}

/// Click-originated actions batched outside the egui closure. Keeps
/// mutable-borrow discipline simple: the closure only writes to this
/// enum; the `app` mutable borrow happens afterwards.
enum ReflogAction {
    Restore { oid: gix::ObjectId },
    OpenRewindConfirm { oid: gix::ObjectId, message: Option<String> },
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

/// The confirmation modal for a destructive reflog rewind.
///
/// Layout is intentionally information-dense: the user is about to
/// rewrite branch history, so every piece of context we can surface
/// without pagination earns its place. Guard against fat-finger is a
/// single "I understand this rewrites history" checkbox — see the
/// `ReflogRewindConfirm` docstring for why we chose checkbox over
/// double-click.
fn reflog_rewind_confirm_modal(ctx: &egui::Context, app: &mut MergeFoxApp) {
    if app.reflog_rewind_confirm.is_none() {
        return;
    }

    let mut open = true;
    // Pull the state OUT temporarily so we can mutate it inside the
    // closure while still having `app` available for `start_reflog_rewind`
    // afterwards.
    let mut state = app
        .reflog_rewind_confirm
        .take()
        .expect("confirm present — just checked above");

    #[derive(Default)]
    struct Decision {
        confirm: bool,
        cancel: bool,
    }
    let mut decision = Decision::default();

    let expected_backup_tag_prefix = "mergefox/reflog-rewind/";

    egui::Window::new("Reset to reflog entry")
        .collapsible(false)
        .resizable(true)
        .default_width(640.0)
        .default_height(520.0)
        .open(&mut open)
        .show(ctx, |ui| {
            let target_short = short_sha(&state.target_oid);
            let head_short = short_sha(&state.preview.current_head);

            let subject_line = state
                .message
                .as_deref()
                .map(primary_message)
                .unwrap_or_else(|| target_short.clone());

            ui.label(
                RichText::new(format!("Reset HEAD to {target_short} — {subject_line}"))
                    .strong()
                    .size(15.0),
            );
            ui.weak(format!(
                "This moves the current branch ref from {head_short} to {target_short}."
            ));

            ui.add_space(6.0);

            // Working-tree guidance.
            if state.preview.working_tree_dirty {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Working tree").strong());
                    ui.weak("will be auto-stashed and restored on top of the new HEAD.");
                });
            } else {
                ui.weak("Working tree is clean — no auto-stash needed.");
            }

            ui.add_space(6.0);

            // Backup-tag guidance.
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Backup tag").strong());
                ui.weak(format!(
                    "under `{expected_backup_tag_prefix}<ISO-timestamp>` will point at {head_short} so you can roll back."
                ));
            });
            ui.weak(
                "To undo later: run `git reset --hard <backup-tag>` or open the Reflog again \
                 — the rewind itself shows up as a fresh reflog entry.",
            );

            ui.add_space(8.0);
            ui.separator();

            // Lost-commits list.
            let lost = &state.preview.lost_commits;
            let total_suffix = if state.preview.preview_truncated {
                format!("{}+ commits", lost.len())
            } else {
                format!("{} commits", lost.len())
            };
            ui.label(
                RichText::new(format!("{total_suffix} will become unreachable"))
                    .color(DESTRUCTIVE_RED)
                    .strong(),
            );
            if lost.is_empty() {
                ui.weak(
                    "No commits will be orphaned — this reset rewinds along the same history, \
                     so it's a moveback rather than a rewrite.",
                );
            } else {
                egui::ScrollArea::vertical()
                    .max_height(220.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for lost_commit in lost {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    RichText::new(short_sha(&lost_commit.oid))
                                        .monospace()
                                        .weak(),
                                );
                                ui.label(primary_message(&lost_commit.subject));
                                ui.weak(format!("· {}", lost_commit.author_date_relative));
                            });
                        }
                        if state.preview.preview_truncated {
                            ui.weak(format!(
                                "… and more (preview capped at {} entries).",
                                crate::git::reflog_rewind::LOST_COMMIT_PREVIEW_CAP
                            ));
                        }
                    });
            }

            ui.add_space(8.0);
            ui.separator();

            // Safety gate: a visible checkbox ("I understand…") has to be
            // ticked before the destructive button enables. Chosen over a
            // two-click confirm because the checkbox is visible from the
            // first frame — the user sees the guard before discovering
            // it by misclicking.
            ui.checkbox(
                &mut state.understood,
                "I understand this rewrites branch history.",
            );

            ui.add_space(4.0);

            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    decision.cancel = true;
                }
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let can_reset = state.understood;
                        let reset_btn = egui::Button::new(
                            RichText::new("Reset (destructive)").color(Color32::WHITE),
                        )
                        .fill(DESTRUCTIVE_RED);
                        ui.add_enabled_ui(can_reset, |ui| {
                            let resp = ui.add(reset_btn);
                            if resp.clicked() {
                                decision.confirm = true;
                            }
                            if !can_reset {
                                resp.on_hover_text(
                                    "Tick the checkbox above to confirm you understand \
                                     that this rewrites branch history.",
                                );
                            }
                        });
                    },
                );
            });
        });

    // Escape behaves as Cancel — but only when the window is still up
    // (if the user ticked the X, `open` is already false and we don't
    // want to double-handle).
    if open && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        decision.cancel = true;
    }

    if decision.confirm {
        // Put the (possibly-mutated, i.e. `understood = true`) state
        // back before calling into the app — `start_reflog_rewind`
        // takes it out itself.
        app.reflog_rewind_confirm = Some(state);
        app.start_reflog_rewind();
        return;
    }
    if decision.cancel || !open {
        // Drop the state, modal closes. No mutation on the repo.
        return;
    }
    // Modal stays open for the next frame — put state back.
    app.reflog_rewind_confirm = Some(state);
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
