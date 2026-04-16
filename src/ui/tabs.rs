//! Repo tab strip.
//!
//! Sits between the top bar and the main panel. One tab per open repo,
//! plus an optional launcher tab for opening / cloning another repo
//! without leaving the current workspace.
//!
//! Keyboard:
//!   * Ctrl+Tab           → next tab   (see app.rs → handle_hotkeys)
//!   * Ctrl+Shift+Tab     → prev tab
//!   * Cmd+W / Ctrl+W     → close active tab

use egui::{RichText, Stroke};

use crate::app::{MergeFoxApp, View};
use crate::config::UiLanguage;

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    let View::Workspace(tabs) = &app.view else {
        return;
    };
    if tabs.tabs.is_empty() && tabs.launcher_tab.is_none() {
        return;
    }

    let entries: Vec<(String, bool)> = tabs
        .tabs
        .iter()
        .enumerate()
        .map(|(i, ws)| {
            (
                ws.tab_title.clone(),
                i == tabs.active && !tabs.launcher_active,
            )
        })
        .collect();
    let has_launcher = tabs.launcher_tab.is_some();
    let launcher_active = tabs.launcher_active;
    let launcher_title = launcher_title(app.config.ui_language.resolved()).to_string();
    let visuals = ctx.style().visuals.clone();

    let mut focus_repo: Option<usize> = None;
    let mut close_repo: Option<usize> = None;
    let mut focus_launcher = false;
    let mut close_launcher = false;
    let mut open_launcher = false;

    egui::TopBottomPanel::top("repo_tabs")
        .exact_height(28.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                egui::ScrollArea::horizontal()
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for (i, (title, active)) in entries.iter().enumerate() {
                                paint_tab(
                                    ui,
                                    title,
                                    *active,
                                    &visuals,
                                    &mut focus_repo,
                                    &mut close_repo,
                                    i,
                                );
                            }
                            if has_launcher {
                                paint_launcher_tab(
                                    ui,
                                    &launcher_title,
                                    launcher_active,
                                    &visuals,
                                    &mut focus_launcher,
                                    &mut close_launcher,
                                );
                            }
                            if ui
                                .small_button("＋")
                                .on_hover_text("Open another repo in a new tab")
                                .clicked()
                            {
                                open_launcher = true;
                            }
                        });
                    });
            });
        });

    if let Some(idx) = close_repo {
        app.close_tab(idx);
    } else if let Some(idx) = focus_repo {
        app.focus_tab(idx);
    }
    if close_launcher {
        app.close_launcher_tab();
    } else if focus_launcher || open_launcher {
        app.open_new_tab();
    }
}

fn paint_tab(
    ui: &mut egui::Ui,
    title: &str,
    active: bool,
    visuals: &egui::Visuals,
    focus: &mut Option<usize>,
    close: &mut Option<usize>,
    idx: usize,
) {
    let (bg, fg) = if active {
        (
            visuals
                .selection
                .bg_fill
                .gamma_multiply(if visuals.dark_mode { 0.58 } else { 0.26 }),
            visuals.strong_text_color(),
        )
    } else {
        (
            visuals.widgets.inactive.weak_bg_fill,
            visuals.widgets.inactive.fg_stroke.color,
        )
    };

    egui::Frame::none()
        .fill(bg)
        .stroke(Stroke::new(
            1.0,
            visuals.widgets.noninteractive.bg_stroke.color,
        ))
        .rounding(2.0)
        .inner_margin(egui::Margin::symmetric(8.0, 3.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let resp = ui.add(
                    egui::Label::new(RichText::new(title).color(fg)).sense(egui::Sense::click()),
                );
                if resp.clicked() {
                    *focus = Some(idx);
                }
                if ui
                    .small_button(RichText::new("×").color(fg))
                    .on_hover_text("Close tab  (Cmd+W)")
                    .clicked()
                {
                    *close = Some(idx);
                }
            });
        });
    ui.add_space(4.0);
}

fn paint_launcher_tab(
    ui: &mut egui::Ui,
    title: &str,
    active: bool,
    visuals: &egui::Visuals,
    focus: &mut bool,
    close: &mut bool,
) {
    let (bg, fg) = if active {
        (
            visuals
                .selection
                .bg_fill
                .gamma_multiply(if visuals.dark_mode { 0.46 } else { 0.20 }),
            visuals.strong_text_color(),
        )
    } else {
        (
            visuals
                .widgets
                .inactive
                .bg_fill
                .gamma_multiply(if visuals.dark_mode { 0.92 } else { 1.0 }),
            visuals.widgets.inactive.fg_stroke.color,
        )
    };

    egui::Frame::none()
        .fill(bg)
        .stroke(Stroke::new(
            1.0,
            visuals.widgets.noninteractive.bg_stroke.color,
        ))
        .rounding(2.0)
        .inner_margin(egui::Margin::symmetric(8.0, 3.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let resp = ui.add(
                    egui::Label::new(RichText::new(title).color(fg)).sense(egui::Sense::click()),
                );
                if resp.clicked() {
                    *focus = true;
                }
                if ui
                    .small_button(RichText::new("×").color(fg))
                    .on_hover_text("Close tab  (Cmd+W)")
                    .clicked()
                {
                    *close = true;
                }
            });
        });
    ui.add_space(4.0);
}

fn launcher_title(language: UiLanguage) -> &'static str {
    match language {
        UiLanguage::Korean => "새 탭",
        UiLanguage::Japanese => "新しいタブ",
        UiLanguage::Chinese => "新标签页",
        UiLanguage::French => "Nouvel onglet",
        UiLanguage::Spanish => "Nueva pestaña",
        _ => "New Tab",
    }
}
