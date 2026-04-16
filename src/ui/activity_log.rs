//! In-app inspector for the MCP activity log.
//!
//! Lets the user scroll through recent git operations, see derived
//! trouble-hints, filter by kind/source, and copy a JSON blob out for
//! pasting into an AI chat. Exactly the same view an MCP client would
//! see over the wire.

use egui::{Color32, RichText, ScrollArea};

use crate::app::{MergeFoxApp, View};
use crate::mcp::types::HintSeverity;
use crate::mcp::{view_for_repo, ActivityLogQuery};

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    if !app.activity_log_open {
        return;
    }

    let mut open = true;
    let mut copy_json: Option<String> = None;
    let mut restore_id: Option<u64> = None;

    // Build the view up-front so the window closure doesn't need a
    // long-lived borrow on `app`.
    let view = {
        let View::Workspace(tabs) = &app.view else {
            app.activity_log_open = false;
            return;
        };
        let ws = tabs.current();
        ws.journal
            .as_ref()
            .map(|j| view_for_repo(j, ActivityLogQuery::recent(200)))
    };

    egui::Window::new("📜 Activity log (MCP)")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(720.0)
        .default_height(520.0)
        .show(ctx, |ui| {
            let Some(view) = &view else {
                ui.weak("Journal unavailable for this repo.");
                return;
            };

            ui.horizontal(|ui| {
                ui.label(format!("{} total entries", view.total));
                if let Some(c) = view.cursor {
                    ui.separator();
                    ui.label(format!("cursor @ #{} (1-based: {})", c, c + 1));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⧉ Copy JSON").on_hover_text("Copy the full view as JSON — paste into an AI chat for help.").clicked() {
                        copy_json = Some(as_json(&view.entries));
                    }
                });
            });
            ui.weak(
                "This is the MCP-shaped view. External tools (AI agents, IDE plugins) will see the same JSON over the wire once the transport lands.",
            );
            ui.separator();

            ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for e in view.entries.iter().rev() {
                    ui.push_id(e.id, |ui| {
                        let outcome_color = match e.outcome {
                            crate::mcp::ActivityOutcome::NoOp => Color32::from_gray(140),
                            crate::mcp::ActivityOutcome::FastForward => Color32::from_rgb(90, 200, 120),
                            crate::mcp::ActivityOutcome::NonLinear => Color32::from_rgb(220, 180, 90),
                            crate::mcp::ActivityOutcome::PossibleConflict => Color32::from_rgb(230, 110, 110),
                        };

                        egui::Frame::none()
                            .fill(Color32::from_rgba_unmultiplied(255, 255, 255, 8))
                            .rounding(2.0)
                            .inner_margin(6.0)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(format!("#{}", e.id)).monospace().weak(),
                                    );
                                    ui.label(RichText::new(&e.label).strong());
                                    ui.label(RichText::new(&e.kind).small().weak());
                                    ui.label(RichText::new(format!("via {}", e.source)).small().weak());
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                RichText::new(format!("{:?}", e.outcome))
                                                    .color(outcome_color)
                                                    .small(),
                                            );
                                            if ui
                                                .small_button("⟲ Restore")
                                                .on_hover_text("Jump repo state back to before this op")
                                                .clicked()
                                            {
                                                restore_id = Some(e.id);
                                            }
                                        },
                                    );
                                });

                                // Ref deltas (compact one-liner)
                                if !e.summary.ref_deltas.is_empty() {
                                    let s = e
                                        .summary
                                        .ref_deltas
                                        .iter()
                                        .map(|d| format!(
                                            "{}: {} → {}",
                                            d.refname,
                                            d.before.as_deref().map(shorten).unwrap_or_else(|| "∅".into()),
                                            d.after.as_deref().map(shorten).unwrap_or_else(|| "∅".into()),
                                        ))
                                        .collect::<Vec<_>>()
                                        .join(" · ");
                                    ui.label(RichText::new(s).monospace().small().weak());
                                }

                                // Trouble hints
                                for h in &e.hints {
                                    let color = match h.severity {
                                        HintSeverity::Info => Color32::from_rgb(120, 170, 220),
                                        HintSeverity::Warn => Color32::from_rgb(230, 190, 90),
                                        HintSeverity::Danger => Color32::from_rgb(240, 110, 110),
                                    };
                                    let icon = match h.severity {
                                        HintSeverity::Info => "ℹ",
                                        HintSeverity::Warn => "⚠",
                                        HintSeverity::Danger => "🛑",
                                    };
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(RichText::new(icon).color(color));
                                        ui.label(RichText::new(&h.message).color(color));
                                    });
                                    ui.indent(("hint_sugg", e.id), |ui| {
                                        ui.weak(&h.suggestion);
                                    });
                                }
                            });
                        ui.add_space(2.0);
                    });
                }
            });
        });

    if !open {
        app.activity_log_open = false;
    }
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.activity_log_open = false;
    }
    if let Some(id) = restore_id {
        app.restore_to_entry(id);
    }
    if let Some(json) = copy_json {
        ctx.copy_text(json.clone());
        app.hud = Some(crate::app::Hud::new(
            format!("Copied {} bytes of activity JSON", json.len()),
            1800,
        ));
    }
}

fn shorten(s: &str) -> String {
    s.chars().take(7).collect()
}

fn as_json(entries: &[crate::mcp::ActivityEntry]) -> String {
    serde_json::to_string_pretty(entries).unwrap_or_else(|_| "[]".into())
}
