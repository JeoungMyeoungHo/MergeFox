//! Small floating HUD shown briefly after undo/redo so the user always
//! sees what just happened and where they now stand on the timeline.

use egui::{Align2, Color32, Context, FontId, Stroke};

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

    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("mergefox-hud"),
    ));
    let visuals = ctx.style().visuals.clone();

    let screen = ctx.screen_rect();
    let pad = egui::Vec2::new(16.0, 12.0);

    // Compose lines
    let mut lines: Vec<(String, Color32, f32)> = Vec::new();
    lines.push((hud.message.clone(), visuals.strong_text_color(), 14.0));
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
            11.0,
        ));
    }

    // Measure
    let mut max_w = 0.0_f32;
    let mut total_h = 0.0_f32;
    let mut galleys = Vec::new();
    for (line, color, size) in &lines {
        let galley = painter.layout_no_wrap(line.clone(), FontId::proportional(*size), *color);
        max_w = max_w.max(galley.size().x);
        total_h += galley.size().y + 4.0;
        galleys.push(galley);
    }
    total_h -= 4.0; // no trailing gap

    let box_size = egui::Vec2::new(max_w, total_h) + pad * 2.0;
    let center = egui::pos2(screen.center().x, screen.top() + 48.0);
    let rect = egui::Rect::from_center_size(center, box_size);

    // Fade
    let age_ms = hud.shown_at.elapsed().as_millis() as u64;
    let fade_in = (age_ms as f32 / 150.0).clamp(0.0, 1.0);
    let fade_out = if age_ms + 300 >= hud.duration_ms {
        ((hud.duration_ms.saturating_sub(age_ms)) as f32 / 300.0).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let alpha = (fade_in * fade_out * 235.0) as u8;

    let bg = visuals.window_fill().gamma_multiply(alpha as f32 / 255.0);
    let stroke = Stroke::new(
        1.0,
        visuals
            .window_stroke()
            .color
            .gamma_multiply(alpha as f32 / 255.0),
    );
    painter.rect(rect, 8.0, bg, stroke);

    // Draw text
    let mut y = rect.min.y + pad.y;
    for (i, galley) in galleys.into_iter().enumerate() {
        let anchor_x = center.x;
        let size = galley.size();
        let pos = egui::pos2(anchor_x - size.x / 2.0, y);
        let mut color = lines[i].1;
        color = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha);
        painter.galley(pos, galley, color);
        y += size.y + 4.0;
    }
    let _ = Align2::CENTER_TOP; // keep import live if needed
}
