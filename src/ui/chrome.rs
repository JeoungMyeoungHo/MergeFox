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

/// Pill-shaped primary button. Filled with the theme accent and used
/// for the single "commit me right now" action that should stand out
/// in the otherwise-monochrome toolbar.
pub fn primary_button(
    ui: &mut egui::Ui,
    settings: &ThemeSettings,
    text: impl Into<String>,
) -> egui::Response {
    let accent = crate::ui::theme::accent(settings);
    let fg = crate::ui::theme::readable_text(accent);
    ui.add(
        egui::Button::new(RichText::new(text.into()).strong().color(fg))
            .fill(accent)
            .stroke(Stroke::new(1.0, accent.gamma_multiply(0.7)))
            .rounding(Rounding::same(12.0)),
    )
}

/// Render a row of mutually-exclusive options as a single pill-
/// shaped segmented control. Each option that's clicked returns its index
/// via the closure; the caller decides what to do with that.
///
/// Visually: one rounded outer frame; each option is a transparent
/// button with no individual rounding, and the active option gets a
/// solid accent fill. Saves the space + visual noise of three separate
/// "selectable_label" calls.
pub fn segmented_selector<S: AsRef<str>>(
    ui: &mut egui::Ui,
    settings: &ThemeSettings,
    options: &[S],
    selected_idx: usize,
) -> Option<usize> {
    let mut clicked: Option<usize> = None;
    let accent = crate::ui::theme::accent(settings);
    let inactive_fg = ui
        .visuals()
        .override_text_color
        .unwrap_or(ui.visuals().widgets.inactive.fg_stroke.color)
        .gamma_multiply(0.85);
    let active_fg = crate::ui::theme::readable_text(accent);

    Frame::none()
        .fill(crate::ui::theme::toolbar_control_fill(settings))
        .stroke(crate::ui::theme::subtle_stroke(settings))
        .rounding(Rounding::same(11.0))
        .inner_margin(Margin::symmetric(2.0, 2.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                for (idx, label) in options.iter().enumerate() {
                    let is_active = idx == selected_idx;
                    let (fill, fg) = if is_active {
                        (accent, active_fg)
                    } else {
                        (Color32::TRANSPARENT, inactive_fg)
                    };
                    let resp = ui.add(
                        egui::Button::new(
                            RichText::new(label.as_ref())
                                .small()
                                .color(fg),
                        )
                        .fill(fill)
                        .stroke(Stroke::NONE)
                        .rounding(Rounding::same(9.0))
                        .min_size(egui::vec2(0.0, 20.0)),
                    );
                    if resp.clicked() && !is_active {
                        clicked = Some(idx);
                    }
                }
            });
        });
    clicked
}

/// A 1-pixel vertical hairline used to break a toolbar into action
/// groups. Reads as a quieter visual delimiter than `ui.separator()`
/// (which is full-width and adds vertical breathing room we don't want
/// in a compact toolbar row).
pub fn toolbar_divider(ui: &mut egui::Ui) {
    let visuals = ui.visuals();
    let fg = visuals
        .override_text_color
        .unwrap_or(visuals.widgets.inactive.fg_stroke.color);
    let bg = visuals.panel_fill;
    let stroke_color = Color32::from_rgba_unmultiplied(
        ((fg.r() as u16 + bg.r() as u16) / 2) as u8,
        ((fg.g() as u16 + bg.g() as u16) / 2) as u8,
        ((fg.b() as u16 + bg.b() as u16) / 2) as u8,
        70,
    );
    let height = ui.spacing().interact_size.y * 0.6;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(1.0, height), egui::Sense::hover());
    let cx = rect.center().x;
    ui.painter().line_segment(
        [egui::pos2(cx, rect.top()), egui::pos2(cx, rect.bottom())],
        Stroke::new(1.0, stroke_color),
    );
    ui.add_space(2.0);
}
