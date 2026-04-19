use egui::{Color32, Context, Id, Rounding, Stroke, Visuals};

use crate::config::{ThemeColor, ThemeSettings};

/// Apply the theme. **Must** be called whenever `ThemeSettings` changes.
///
/// The actual `set_visuals` / `set_style` work is expensive enough that we
/// memoize it: `update()` calls this on every frame, but 99.9 % of those
/// calls are idempotent (the theme hasn't moved) so we short-circuit on
/// a cheap hash compare. Without this short-circuit, egui was rebuilding
/// its style struct + re-setting visuals every paint, which on a busy
/// click-repaint cycle piled up enough to starve input events for ~1 s.
pub fn apply(ctx: &Context, settings: &ThemeSettings) {
    let hash = settings_hash(settings);
    let cache_id = Id::new("__mergefox_theme_hash");
    let hash_unchanged = ctx.data(|d| d.get_temp::<u64>(cache_id)) == Some(hash);

    // Even when the user's ThemeSettings hasn't changed, egui may
    // reset its visuals back to system defaults on events like:
    //   * macOS Appearance toggle (Ventura+)
    //   * Window regaining focus after a system-wide dark/light switch
    //   * eframe internally calling `ctx.set_visuals(Visuals::dark())`
    //
    // Detect this by comparing egui's live `dark_mode` flag against
    // what our palette dictates. If they disagree, something outside
    // our control flipped the visuals and we need to re-stamp ours.
    let expected_dark = settings.active_palette().is_dark();
    let current_dark = ctx.style().visuals.dark_mode;
    let visuals_drifted = current_dark != expected_dark;

    if hash_unchanged && !visuals_drifted {
        return;
    }
    apply_impl(ctx, settings);
    ctx.data_mut(|d| d.insert_temp(cache_id, hash));
}

fn settings_hash(settings: &ThemeSettings) -> u64 {
    // A cheap structural hash — we don't need cryptographic strength,
    // we need "did any knob move". serde_json gives us a stable
    // representation without us having to hand-maintain a `Hash` impl
    // that stays in sync with every new field on ThemeSettings.
    use std::hash::{Hash, Hasher};
    let json = serde_json::to_string(settings).unwrap_or_default();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    json.hash(&mut h);
    h.finish()
}

fn apply_impl(ctx: &Context, settings: &ThemeSettings) {
    let palette = settings.active_palette();
    let background = palette.background.to_color32();
    let foreground = palette.foreground.to_color32();
    let accent = palette.accent.to_color32();
    let contrast = palette.contrast as f32 / 100.0;
    let dark = palette.is_dark();

    let mut visuals = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    let panel_surface = surface(background, foreground, contrast, dark, 0.06, 0.16);
    let faint_surface = surface(panel_surface, foreground, contrast, dark, 0.03, 0.08);
    let extreme_surface = surface(panel_surface, foreground, contrast, dark, 0.11, 0.20);
    let stroke_color = mix(panel_surface, foreground, if dark { 0.26 } else { 0.18 });
    let hovered_fill = mix(panel_surface, accent, if dark { 0.18 } else { 0.10 });
    let active_fill = mix(accent, background, if dark { 0.18 } else { 0.12 });
    let panel_fill = if palette.translucent_panels {
        alpha(background, if dark { 232 } else { 242 })
    } else {
        background
    };

    visuals.dark_mode = dark;
    visuals.override_text_color = Some(foreground);
    visuals.hyperlink_color = accent;
    visuals.faint_bg_color = faint_surface;
    visuals.extreme_bg_color = extreme_surface;
    visuals.code_bg_color = faint_surface;
    visuals.warn_fg_color = ThemeColor::rgb(240, 180, 96).to_color32();
    visuals.error_fg_color = ThemeColor::rgb(235, 108, 108).to_color32();
    visuals.window_fill = panel_surface;
    visuals.panel_fill = panel_fill;
    visuals.window_stroke = Stroke::new(1.0, stroke_color);
    // Tightened corner radii for a less "app-store macaron" look.
    // Earlier 8–10 px felt floaty against tight info-dense screens
    // (graph, diff). 3–5 px keeps edges visibly rounded but reads as
    // precise rather than decorative.
    visuals.window_rounding = Rounding::same(5.0);
    visuals.menu_rounding = Rounding::same(4.0);

    visuals.selection.bg_fill = accent;
    visuals.selection.stroke = Stroke::new(1.0, readable_text(accent));

    let widget_rounding = Rounding::same(3.0);

    visuals.widgets.noninteractive.bg_fill = panel_surface;
    visuals.widgets.noninteractive.weak_bg_fill = faint_surface;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, stroke_color);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, foreground);
    visuals.widgets.noninteractive.rounding = widget_rounding;

    visuals.widgets.inactive.bg_fill = panel_surface;
    visuals.widgets.inactive.weak_bg_fill = faint_surface;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, stroke_color);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, foreground.gamma_multiply(0.96));
    visuals.widgets.inactive.rounding = widget_rounding;

    visuals.widgets.hovered.bg_fill = hovered_fill;
    visuals.widgets.hovered.weak_bg_fill = hovered_fill;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, mix(stroke_color, accent, 0.38));
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.2, readable_text(hovered_fill));
    visuals.widgets.hovered.rounding = widget_rounding;

    visuals.widgets.active.bg_fill = active_fill;
    visuals.widgets.active.weak_bg_fill = active_fill;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, accent);
    visuals.widgets.active.fg_stroke = Stroke::new(1.2, readable_text(active_fill));
    visuals.widgets.active.rounding = widget_rounding;

    visuals.widgets.open = visuals.widgets.active;
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    // Dense IDE-style spacing. The earlier 10×8 button padding + 40×30
    // minimum hit-target matched macOS system dialog feel, but on a
    // git client with 6+ toolbar buttons + top-bar + tab strip + side
    // panel controls it ate the horizontal space so the main view wrapped
    // every line. Tightened to something closer to an IDE-density
    // baseline while still meeting Apple's 24-px minimum hit target
    // on the y-axis.
    style.spacing.button_padding = egui::vec2(7.0, 4.0);
    style.spacing.interact_size = egui::vec2(28.0, 24.0);
    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    ctx.set_style(style);
}

pub fn sidebar_fill(settings: &ThemeSettings) -> Color32 {
    let palette = settings.active_palette();
    let background = palette.background.to_color32();
    if palette.translucent_panels {
        alpha(background, if palette.is_dark() { 220 } else { 238 })
    } else {
        background
    }
}

fn surface(
    background: Color32,
    foreground: Color32,
    contrast: f32,
    dark: bool,
    low: f32,
    high: f32,
) -> Color32 {
    let amount = low + (high - low) * contrast.clamp(0.0, 1.0);
    let mixed = mix(background, foreground, amount);
    if dark {
        alpha(mixed, 244)
    } else {
        mixed
    }
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |lhs: u8, rhs: u8| ((lhs as f32) + ((rhs as f32) - (lhs as f32)) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        lerp(a.r(), b.r()),
        lerp(a.g(), b.g()),
        lerp(a.b(), b.b()),
        lerp(a.a(), b.a()),
    )
}

fn alpha(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

fn readable_text(color: Color32) -> Color32 {
    let luminance = ThemeColor::from_color32(color).luminance();
    if luminance > 0.45 {
        Color32::from_rgb(20, 20, 24)
    } else {
        Color32::from_rgb(250, 248, 242)
    }
}
