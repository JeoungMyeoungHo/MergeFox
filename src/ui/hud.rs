//! Small floating HUD shown briefly after undo/redo so the user always
//! sees what just happened and where they now stand on the timeline.

use egui::{Align2, Color32, Context, RichText, Stroke};

use crate::app::MergeFoxApp;
use crate::app::View;

pub fn show(ctx: &Context, app: &mut MergeFoxApp) {
    if app.hud.as_ref().map(|h| h.expired()).unwrap_or(true) {
        app.hud = None;
        return;
    }
    let Some(hud) = app.hud.as_ref() else {
        return;
    };

    let cursor_info = if let View::Workspace(tabs) = &app.view {
        tabs.current().journal.as_ref().map(|j| {
            let total = j.entries.len();
            let pos = j.cursor.map(|i| i + 1).unwrap_or(0);
            let can_undo = pos > 0;
            let can_redo = pos < total;
            (pos, total, can_undo, can_redo)
        })
    } else {
        None
    };

    let visuals = ctx.style().visuals.clone();
    let mut clicked_action = None;

    let mut lines: Vec<(String, Color32)> = Vec::new();
    lines.push((hud.message.clone(), visuals.strong_text_color()));
    if let Some((pos, total, can_undo, can_redo)) = cursor_info {
        lines.push((
            format!(
                "position {pos} / {total}    ↶ {}    {} ↷",
                if can_undo {
                    "undo available"
                } else {
                    "no undo"
                },
                if can_redo {
                    "redo available"
                } else {
                    "no redo"
                },
            ),
            visuals.weak_text_color(),
        ));
    }

    let age_ms = hud.shown_at.elapsed().as_millis() as u64;
    let fade_in = (age_ms as f32 / 150.0).clamp(0.0, 1.0);
    let fade_out = if age_ms + 300 >= hud.duration_ms {
        ((hud.duration_ms.saturating_sub(age_ms)) as f32 / 300.0).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let alpha = (fade_in * fade_out * 235.0) as u8;

    let bg = visuals.window_fill().gamma_multiply(alpha as f32 / 255.0);
    let frame_stroke = Stroke::new(
        1.0,
        visuals
            .window_stroke()
            .color
            .gamma_multiply(alpha as f32 / 255.0),
    );
    egui::Area::new(egui::Id::new("mergefox-hud"))
        .order(egui::Order::Foreground)
        .anchor(Align2::CENTER_TOP, [0.0, 48.0])
        .show(ctx, |ui| {
            egui::Frame::window(ui.style())
                .fill(bg)
                .stroke(frame_stroke)
                .rounding(8.0)
                .inner_margin(egui::Margin::symmetric(16.0, 12.0))
                .show(ui, |ui| {
                    ui.set_max_width(520.0);
                    ui.vertical_centered(|ui| {
                        for (idx, (line, color)) in lines.iter().enumerate() {
                            let color = Color32::from_rgba_unmultiplied(
                                color.r(),
                                color.g(),
                                color.b(),
                                alpha,
                            );
                            let text = if idx == 0 {
                                RichText::new(line).color(color).strong().size(14.0)
                            } else {
                                RichText::new(line).color(color).size(11.0)
                            };
                            ui.label(text);
                        }
                        if let Some(action) = hud.action.clone() {
                            ui.add_space(6.0);
                            if ui
                                .button(RichText::new(action.label).color(visuals.hyperlink_color))
                                .clicked()
                            {
                                clicked_action = Some(action.intent);
                            }
                        }
                    });
                });
        });

    if let Some(action) = clicked_action {
        match action {
            crate::app::HudIntent::OpenSettings(section) => app.open_settings_section(section),
        }
        app.hud = None;
    }
}
