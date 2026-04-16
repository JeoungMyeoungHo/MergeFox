//! Settings window — left sidebar categories, right-side section panels.
//!
//! Layout rationale
//! ----------------
//! One big scrolling window stops working as soon as the app accumulates
//! settings beyond the current couple of sections. We split into:
//!
//!   * `mod.rs` — the window shell, sidebar, section dispatch, keyboard
//!     handling, and the `Feedback` banner. Nothing domain-specific lives
//!     here.
//!   * `general.rs`, `repo.rs` — one file per category. Each exposes a
//!     `pub fn show(ui, app) -> Option<SectionIntent>` that returns what
//!     the user asked to do; the top-level shell dispatches the intent so
//!     borrow-scope is clear and every section stays independently
//!     testable.
//!
//! Save model
//! ----------
//! We use **immediate save** everywhere (matching macOS System Settings
//! and the existing Remote-URL behaviour). There is no bottom "Save" /
//! "Cancel" — the user changes a field, it persists immediately, and the
//! banner shows whether it worked. This removes the previous ambiguity
//! where language/remote/strategy required pressing Save while URL edits
//! saved instantly.
//!
//! Feedback
//! --------
//! `Feedback` is a unified success/error banner. Prior code had parallel
//! `notice` + `last_error` fields that could both be set simultaneously
//! after a refresh-after-save failure; the enum forces one or the other.

use egui::{Color32, RichText};

use crate::app::MergeFoxApp;
use crate::config::UiLanguage;

pub mod ai;
pub mod general;
pub mod integrations;
pub mod repo;

const WINDOW_MIN_WIDTH: f32 = 760.0;
const WINDOW_MIN_HEIGHT: f32 = 360.0;
const WINDOW_DEFAULT_WIDTH: f32 = 860.0;
const WINDOW_DEFAULT_HEIGHT: f32 = 460.0;
const SIDEBAR_WIDTH: f32 = 216.0;
const BODY_MIN_WIDTH: f32 = 320.0;

/// Which left-sidebar category is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    General,
    Repository,
    Integrations,
    Ai,
}

impl SettingsSection {
    fn all() -> &'static [Self] {
        &[
            Self::General,
            Self::Repository,
            Self::Integrations,
            Self::Ai,
        ]
    }

    fn label(self, lang: UiLanguage) -> &'static str {
        match (self, lang) {
            (Self::General, UiLanguage::Korean) => "일반",
            (Self::General, UiLanguage::Japanese) => "一般",
            (Self::General, UiLanguage::Chinese) => "常规",
            (Self::General, UiLanguage::French) => "Général",
            (Self::General, UiLanguage::Spanish) => "General",
            (Self::General, _) => "General",
            (Self::Repository, UiLanguage::Korean) => "저장소",
            (Self::Repository, UiLanguage::Japanese) => "リポジトリ",
            (Self::Repository, UiLanguage::Chinese) => "仓库",
            (Self::Repository, UiLanguage::French) => "Dépôt",
            (Self::Repository, UiLanguage::Spanish) => "Repositorio",
            (Self::Repository, _) => "Repository",
            (Self::Integrations, UiLanguage::Korean) => "연동",
            (Self::Integrations, UiLanguage::Japanese) => "連携",
            (Self::Integrations, UiLanguage::Chinese) => "集成",
            (Self::Integrations, UiLanguage::French) => "Intégrations",
            (Self::Integrations, UiLanguage::Spanish) => "Integraciones",
            (Self::Integrations, _) => "Integrations",
            (Self::Ai, UiLanguage::Korean) => "AI",
            (Self::Ai, _) => "AI",
        }
    }
}

/// Feedback banner — one variant at a time, never both.
#[derive(Debug, Clone)]
pub enum Feedback {
    Ok(String),
    Err(String),
}

impl Feedback {
    pub fn ok(msg: impl Into<String>) -> Self {
        Self::Ok(msg.into())
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self::Err(msg.into())
    }
}

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    if !app.settings_open {
        return;
    }
    if app.settings_modal.is_none() {
        app.open_settings();
    }
    if app.settings_modal.is_none() {
        app.settings_open = false;
        return;
    }

    let language = app
        .settings_modal
        .as_ref()
        .map(|m| m.language.resolved())
        .unwrap_or_else(|| app.config.ui_language.resolved());
    let title = window_title(language);
    let labels = chrome_labels(language);
    let viewport = ctx.available_rect();
    let max_width = (viewport.width() - 48.0).max(320.0);
    let max_height = (viewport.height() - 48.0).max(240.0);
    let min_width = WINDOW_MIN_WIDTH.min(max_width);
    let min_height = WINDOW_MIN_HEIGHT.min(max_height);
    let default_width = WINDOW_DEFAULT_WIDTH.clamp(min_width, max_width);
    let default_height = WINDOW_DEFAULT_HEIGHT.clamp(min_height, max_height);
    let default_pos = egui::pos2(
        viewport.center().x - default_width * 0.5,
        viewport.center().y - default_height * 0.5,
    );

    let mut open = true;
    let mut requested_close = false;

    egui::Window::new(title)
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_pos(default_pos)
        .default_width(default_width)
        .default_height(default_height)
        .min_width(min_width)
        .min_height(min_height)
        .max_width(max_width)
        .max_height(max_height)
        .constrain_to(viewport)
        .show(ctx, |ui| {
            ui.set_min_width(min_width - 24.0);
            let footer_height = 36.0;
            let body_height = (ui.available_height() - footer_height).max(160.0);

            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(SIDEBAR_WIDTH, body_height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        render_sidebar(ui, app, language);
                    },
                );

                ui.separator();

                let body_width = ui.available_width().max(BODY_MIN_WIDTH);
                ui.allocate_ui_with_layout(
                    egui::vec2(body_width, body_height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        render_body(ui, app);
                    },
                );
            });

            ui.add_space(4.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                render_feedback(ui, app);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(labels.close).clicked() {
                        requested_close = true;
                    }
                });
            });
        });

    // Escape closes, but only when no text field is focused — otherwise an
    // IME composition (e.g. Korean Hangul) would be cancelled by closing
    // the window instead of cancelling the composition.
    let wants_kb = ctx.wants_keyboard_input();
    let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !wants_kb;

    if !open || requested_close || escape {
        app.settings_open = false;
        app.settings_modal = None;
        app.provider_oauth_start_task = None;
        app.provider_oauth_poll_task = None;
    }
}

fn render_sidebar(ui: &mut egui::Ui, app: &mut MergeFoxApp, language: UiLanguage) {
    ui.add_space(4.0);
    ui.set_width(ui.available_width());
    // Each tab gets a uniform full-sidebar-width row so short labels
    // ("AI") don't render as a tiny 30 px button next to longer ones
    // ("Integrations" / "저장소"). Matches System Settings / VS Code
    // navigation lists — clickable target is always a predictable strip.
    let row_width = ui.available_width();
    let row_height = 30.0;
    let row_size = egui::vec2(row_width, row_height);
    for section in SettingsSection::all() {
        let Some(modal) = app.settings_modal.as_mut() else {
            return;
        };
        let selected = modal.section == *section;
        let label = section.label(language);
        let text = if selected {
            RichText::new(label).strong()
        } else {
            RichText::new(label)
        };
        let resp = ui.add_sized(row_size, egui::SelectableLabel::new(selected, text));
        if resp.clicked() {
            modal.section = *section;
            // Clear stale banners when switching sections so feedback
            // from an unrelated operation doesn't linger.
            modal.feedback = None;
        }
    }
}

fn render_body(ui: &mut egui::Ui, app: &mut MergeFoxApp) {
    let Some(section) = app.settings_modal.as_ref().map(|m| m.section) else {
        return;
    };
    egui::ScrollArea::vertical()
        .id_salt("settings_body")
        .auto_shrink([false; 2])
        .max_height(ui.available_height())
        .show(ui, |ui| match section {
            SettingsSection::General => general::show(ui, app),
            SettingsSection::Repository => repo::show(ui, app),
            SettingsSection::Integrations => integrations::show(ui, app),
            SettingsSection::Ai => ai::show(ui, app),
        });
}

fn render_feedback(ui: &mut egui::Ui, app: &MergeFoxApp) {
    let Some(modal) = app.settings_modal.as_ref() else {
        return;
    };
    match &modal.feedback {
        Some(Feedback::Ok(msg)) => {
            ui.colored_label(Color32::LIGHT_GREEN, msg);
        }
        Some(Feedback::Err(msg)) => {
            ui.colored_label(Color32::LIGHT_RED, msg);
        }
        None => {}
    }
}

struct ChromeLabels {
    close: &'static str,
}

fn chrome_labels(lang: UiLanguage) -> ChromeLabels {
    match lang {
        UiLanguage::Korean => ChromeLabels { close: "닫기" },
        UiLanguage::Japanese => ChromeLabels { close: "閉じる" },
        UiLanguage::Chinese => ChromeLabels { close: "关闭" },
        UiLanguage::French => ChromeLabels { close: "Fermer" },
        UiLanguage::Spanish => ChromeLabels { close: "Cerrar" },
        _ => ChromeLabels { close: "Close" },
    }
}

fn window_title(lang: UiLanguage) -> &'static str {
    match lang {
        UiLanguage::Korean => "설정",
        UiLanguage::Japanese => "設定",
        UiLanguage::Chinese => "设置",
        UiLanguage::French => "Paramètres",
        UiLanguage::Spanish => "Ajustes",
        _ => "Settings",
    }
}

// ============================================================
// Shared helpers for section modules.
// ============================================================

/// Persist the Config and show a matching feedback banner.
pub(super) fn persist_config(app: &mut MergeFoxApp, ok_msg: &str) {
    let result = app.config.save();
    let Some(modal) = app.settings_modal.as_mut() else {
        return;
    };
    match result {
        Ok(()) => {
            modal.feedback = Some(Feedback::ok(ok_msg));
            app.hud = Some(crate::app::Hud::new(ok_msg, 1600));
        }
        Err(err) => {
            modal.feedback = Some(Feedback::err(format!("save settings: {err:#}")));
        }
    }
}

/// Run a closure with the repo the settings window is bound to, guarding
/// against the active tab having changed since the window was opened.
pub(super) fn with_settings_repo<T>(
    app: &mut MergeFoxApp,
    f: impl FnOnce(&crate::git::Repo) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    use crate::app::View;
    use anyhow::{anyhow, bail};

    let target = app
        .settings_modal
        .as_ref()
        .and_then(|modal| modal.repo_path.clone())
        .ok_or_else(|| anyhow!("no repository is attached to this settings window"))?;
    let View::Workspace(tabs) = &mut app.view else {
        bail!("no repository is currently open");
    };
    let ws = tabs.current_mut();
    if ws.repo.path() != target.as_path() {
        bail!("current tab changed; reopen Settings for this repository");
    }
    f(&ws.repo)
}

/// Re-read the repo's remotes and refresh the modal's `remotes` list.
/// Used after add/delete/update to keep the on-screen state in sync.
pub(super) fn refresh_modal_remotes(app: &mut MergeFoxApp) -> anyhow::Result<()> {
    let remotes = with_settings_repo(app, |repo| repo.list_remotes())?;
    let Some(modal) = app.settings_modal.as_mut() else {
        return Ok(());
    };
    modal.remotes = remotes
        .into_iter()
        .map(|remote| crate::app::RemoteDraft {
            name: remote.name,
            fetch_url: remote.fetch_url.unwrap_or_default(),
            push_url: remote.push_url.unwrap_or_default(),
        })
        .collect();
    Ok(())
}

/// Common post-processing for remote CRUD: refresh the list, then write
/// the feedback banner. Any error in either step becomes the banner.
pub(super) fn finish_repo_update(
    app: &mut MergeFoxApp,
    result: anyhow::Result<()>,
    success_label: &str,
) {
    match result {
        Ok(()) => {
            let refresh = refresh_modal_remotes(app);
            let Some(modal) = app.settings_modal.as_mut() else {
                return;
            };
            match refresh {
                Ok(()) => {
                    modal.feedback = Some(Feedback::ok(success_label));
                    app.hud = Some(crate::app::Hud::new(success_label, 1600));
                }
                Err(err) => {
                    modal.feedback = Some(Feedback::err(format!("refresh remotes: {err:#}")));
                }
            }
        }
        Err(err) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::err(format!("{err:#}")));
            }
        }
    }
}
