//! Shared egui chrome primitives.
//!
//! These are deliberately small wrappers rather than a separate widget
//! framework. The goal is to get CSS-like consistency (tokens, pills, toolbar
//! rows) while keeping the native egui memory profile.

use egui::{Color32, Frame, Margin, RichText, Rounding, Stroke};

use crate::config::ThemeSettings;

pub fn toolbar_frame(settings: &ThemeSettings) -> Frame {
    Frame::none()
        .fill(crate::ui::theme::top_bar_fill(settings))
        .stroke(crate::ui::theme::subtle_stroke(settings))
        .inner_margin(Margin::symmetric(8.0, 5.0))
}

pub fn apply_toolbar_visuals(ui: &mut egui::Ui, settings: &ThemeSettings) {
    let mut style = (**ui.style()).clone();
    let stroke = crate::ui::theme::subtle_stroke(settings);
    let fill = crate::ui::theme::toolbar_control_fill(settings);
    let hover = crate::ui::theme::toolbar_control_hover_fill(settings);
    let active = crate::ui::theme::toolbar_control_active_fill(settings);
    let text = style
        .visuals
        .override_text_color
        .unwrap_or(style.visuals.widgets.inactive.fg_stroke.color);

    style.visuals.widgets.inactive.bg_fill = fill;
    style.visuals.widgets.inactive.weak_bg_fill = fill;
    style.visuals.widgets.inactive.bg_stroke = stroke;
    style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, text.gamma_multiply(0.90));

    style.visuals.widgets.hovered.bg_fill = hover;
    style.visuals.widgets.hovered.weak_bg_fill = hover;
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, crate::ui::theme::accent(settings));
    style.visuals.widgets.hovered.fg_stroke = Stroke::new(1.1, text);

    style.visuals.widgets.active.bg_fill = active;
    style.visuals.widgets.active.weak_bg_fill = active;
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.0, crate::ui::theme::accent(settings));
    style.visuals.widgets.active.fg_stroke = Stroke::new(1.1, text);

    ui.set_style(style);
}

pub fn center_frame(settings: &ThemeSettings) -> Frame {
    Frame::none()
        .fill(crate::ui::theme::workspace_fill(settings))
        .inner_margin(Margin::symmetric(8.0, 8.0))
}

pub fn section_title(ui: &mut egui::Ui, title: &str) {
    ui.add_space(2.0);
    ui.label(
        RichText::new(title.to_ascii_uppercase())
            .size(11.0)
            .strong()
            .color(crate::ui::theme::muted_text(
                ui.ctx().style().visuals.override_text_color,
            )),
    );
    ui.add_space(2.0);
}

pub fn pill(ui: &mut egui::Ui, text: impl Into<String>, color: Color32) -> egui::Response {
    let text = text.into();
    Frame::none()
        .fill(Color32::from_rgba_unmultiplied(
            color.r(),
            color.g(),
            color.b(),
            24,
        ))
        .stroke(Stroke::new(
            1.0,
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 92),
        ))
        .rounding(Rounding::same(3.0))
        .inner_margin(Margin::symmetric(5.0, 1.0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().strong().color(color));
        })
        .response
}

pub fn muted_pill(ui: &mut egui::Ui, text: impl Into<String>) -> egui::Response {
    let visuals = ui.visuals();
    let color = visuals
        .override_text_color
        .unwrap_or(visuals.widgets.inactive.fg_stroke.color)
        .gamma_multiply(0.68);
    pill(ui, text, color)
}

pub fn toolbar_button(ui: &mut egui::Ui, text: impl Into<String>) -> egui::Response {
    ui.add_sized(
        egui::vec2(74.0, 24.0),
        egui::Button::new(RichText::new(text.into()).small()),
    )
}
