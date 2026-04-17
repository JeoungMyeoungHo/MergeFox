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

use std::fs;
use std::path::{Path, PathBuf};

use egui::{Color32, RichText, TextEdit};

use crate::app::MergeFoxApp;
use crate::config::{
    CloneDefaults, Config, RepoSettings, SettingsWindowState, ThemeSettings, UiLanguage,
};

pub mod about;
pub mod ai;
pub mod general;
pub mod integrations;
pub mod repo;

const WINDOW_MIN_WIDTH: f32 = 760.0;
const WINDOW_MIN_HEIGHT: f32 = 360.0;
const SIDEBAR_WIDTH: f32 = 216.0;
const BODY_MIN_WIDTH: f32 = 320.0;

/// Which left-sidebar category is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    General,
    Repository,
    Integrations,
    Ai,
    About,
}

impl SettingsSection {
    fn all() -> &'static [Self] {
        &[
            Self::General,
            Self::Repository,
            Self::Integrations,
            Self::Ai,
            Self::About,
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
            (Self::About, UiLanguage::Korean) => "정보",
            (Self::About, UiLanguage::Japanese) => "情報",
            (Self::About, UiLanguage::Chinese) => "关于",
            (Self::About, UiLanguage::French) => "À propos",
            (Self::About, UiLanguage::Spanish) => "Acerca de",
            (Self::About, _) => "About",
        }
    }

    fn search_keywords(self) -> &'static [&'static str] {
        match self {
            Self::General => &[
                "language",
                "theme",
                "palette",
                "git runtime",
                "git identity",
                "clone defaults",
                "ui",
            ],
            Self::Repository => &[
                "default remote",
                "pull strategy",
                "provider account",
                "remotes",
                "worktrees",
                "repository",
            ],
            Self::Integrations => &[
                "providers",
                "github",
                "gitlab",
                "codeberg",
                "bitbucket",
                "azure",
                "gitea",
                "oauth",
                "pat",
                "ssh",
                "token",
            ],
            Self::Ai => &[
                "ai",
                "endpoint",
                "model",
                "api key",
                "openai",
                "anthropic",
                "ollama",
                "grammar",
                "streaming",
            ],
            Self::About => &[
                "about",
                "diagnostics",
                "logs",
                "config",
                "version",
                "build",
                "commit",
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsResetScope {
    Section(SettingsSection),
    Global,
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
    let preferred_size = app
        .settings_modal
        .as_ref()
        .and_then(|modal| modal.window_size.clone())
        .unwrap_or_else(|| app.config.settings_window.clone());
    let default_width = preferred_size.width.clamp(min_width, max_width);
    let default_height = preferred_size.height.clamp(min_height, max_height);
    let default_pos = egui::pos2(
        viewport.center().x - default_width * 0.5,
        viewport.center().y - default_height * 0.5,
    );

    let mut open = true;
    let mut requested_close = false;

    let window = egui::Window::new(title)
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
            render_repo_context_bar(ui, app, language);
            ui.add_space(4.0);
            ui.separator();
            let footer_height = 36.0;
            let body_height = (ui.available_height() - footer_height - 8.0).max(160.0);

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
                render_reset_actions(ui, app, &labels);
                ui.add_space(8.0);
                render_feedback(ui, app);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(labels.close).clicked() {
                        requested_close = true;
                    }
                });
            });
        });

    if let Some(window) = window {
        let size = window.response.rect.size();
        if let Some(modal) = app.settings_modal.as_mut() {
            modal.window_size = Some(SettingsWindowState {
                width: size.x,
                height: size.y,
            });
        }
    }

    // Escape closes, but only when no text field is focused — otherwise an
    // IME composition (e.g. Korean Hangul) would be cancelled by closing
    // the window instead of cancelling the composition.
    let wants_kb = ctx.wants_keyboard_input();
    let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !wants_kb;

    if !open || requested_close || escape {
        persist_window_size(app);
        app.settings_open = false;
        app.settings_modal = None;
        app.provider_oauth_start_task = None;
        app.provider_oauth_poll_task = None;
    }

    show_reset_confirm(ctx, app, &labels);
}

fn render_sidebar(ui: &mut egui::Ui, app: &mut MergeFoxApp, language: UiLanguage) {
    ui.add_space(4.0);
    ui.set_width(ui.available_width());
    if let Some(modal) = app.settings_modal.as_mut() {
        ui.add(
            TextEdit::singleline(&mut modal.search_query)
                .desired_width(f32::INFINITY)
                .hint_text(chrome_labels(language).search_hint),
        );
        ui.add_space(8.0);
    }

    let visible_sections = matching_sections(app, language);
    let mut current_section = match app.settings_modal.as_ref() {
        Some(modal) => modal.section,
        None => return,
    };
    if !visible_sections.contains(&current_section) {
        if let Some(first) = visible_sections.first().copied() {
            current_section = first;
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.section = first;
            }
        }
    }

    // Each tab gets a uniform full-sidebar-width row so short labels
    // ("AI") don't render as a tiny 30 px button next to longer ones
    // ("Integrations" / "저장소"). Matches System Settings / VS Code
    // navigation lists — clickable target is always a predictable strip.
    let row_width = ui.available_width();
    let row_height = 30.0;
    let row_size = egui::vec2(row_width, row_height);
    let mut clicked_section: Option<SettingsSection> = None;
    for &section in &visible_sections {
        let selected = current_section == section;
        let changed = section_is_changed(app, section);
        let label = section.label(language);
        let text = if selected || changed {
            RichText::new(label).strong()
        } else {
            RichText::new(label)
        };
        let resp = ui.add_sized(row_size, egui::SelectableLabel::new(selected, text));
        let mut badge_rect = resp.rect;
        badge_rect.set_left(resp.rect.right() - 70.0);
        ui.scope_builder(egui::UiBuilder::new().max_rect(badge_rect), |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if changed {
                    ui.label(
                        RichText::new(chrome_labels(language).changed_badge)
                            .small()
                            .color(Color32::from_rgb(255, 171, 92)),
                    );
                }
            });
        });
        if resp.clicked() {
            clicked_section = Some(section);
        }
    }

    if visible_sections.is_empty() {
        ui.add_space(8.0);
        ui.weak(chrome_labels(language).search_empty);
    }

    if let Some(section) = clicked_section {
        if let Some(modal) = app.settings_modal.as_mut() {
            modal.section = section;
            // Clear stale banners when switching sections so feedback
            // from an unrelated operation doesn't linger.
            modal.feedback = None;
        }
    }
}

fn render_repo_context_bar(ui: &mut egui::Ui, app: &mut MergeFoxApp, language: UiLanguage) {
    let labels = chrome_labels(language);
    let mut switch_repo: Option<Option<PathBuf>> = None;
    let open_repos = open_repo_paths(app);
    let active_repo = active_repo_path(app);
    let selected_repo = app
        .settings_modal
        .as_ref()
        .and_then(|modal| modal.repo_path.clone());

    ui.horizontal(|ui| {
        ui.label(RichText::new(labels.repo_context).strong());
        if open_repos.is_empty() {
            ui.weak(labels.repo_context_none);
            return;
        }

        egui::ComboBox::from_id_salt("settings_repo_context")
            .selected_text(repo_choice_label(selected_repo.as_deref(), &labels))
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(selected_repo.is_none(), labels.repo_context_none)
                    .clicked()
                {
                    switch_repo = Some(None);
                }
                for repo in &open_repos {
                    let selected = selected_repo.as_deref() == Some(repo.as_path());
                    let label = repo_choice_label(Some(repo.as_path()), &labels);
                    if ui.selectable_label(selected, label).clicked() {
                        switch_repo = Some(Some(repo.clone()));
                    }
                }
            });

        if active_repo.as_deref() != selected_repo.as_deref() {
            if let Some(active_repo) = active_repo.as_ref() {
                ui.separator();
                ui.weak(format!(
                    "{} {}",
                    labels.repo_context_active,
                    active_repo.display()
                ));
                if ui.small_button(labels.repo_context_use_active).clicked() {
                    switch_repo = Some(Some(active_repo.clone()));
                }
            }
        }
    });

    if let Some(repo_path) = selected_repo.as_ref() {
        ui.weak(format!(
            "{} {}",
            labels.repo_context_bound,
            repo_path.display()
        ));
    }

    if let Some(repo_path) = switch_repo {
        switch_settings_repo(app, repo_path);
    }
}

fn render_body(ui: &mut egui::Ui, app: &mut MergeFoxApp) {
    let language = app
        .settings_modal
        .as_ref()
        .map(|m| m.language.resolved())
        .unwrap_or_else(|| app.config.ui_language.resolved());
    let visible_sections = matching_sections(app, language);
    if visible_sections.is_empty() {
        ui.weak(chrome_labels(language).search_empty);
        return;
    }
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
            SettingsSection::About => about::show(ui, app),
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
    search_hint: &'static str,
    search_empty: &'static str,
    changed_badge: &'static str,
    repo_context: &'static str,
    repo_context_none: &'static str,
    repo_context_active: &'static str,
    repo_context_bound: &'static str,
    repo_context_use_active: &'static str,
    reset_section: &'static str,
    reset_all: &'static str,
    reset_title: &'static str,
    reset_cancel: &'static str,
    reset_confirm: &'static str,
    export_json: &'static str,
    import_json: &'static str,
}

fn chrome_labels(lang: UiLanguage) -> ChromeLabels {
    match lang {
        UiLanguage::Korean => ChromeLabels {
            close: "닫기",
            search_hint: "설정 검색",
            search_empty: "검색과 일치하는 설정 섹션이 없습니다.",
            changed_badge: "변경됨",
            repo_context: "리포 컨텍스트",
            repo_context_none: "열린 리포 없음",
            repo_context_active: "현재 탭:",
            repo_context_bound: "이 창이 편집 중인 리포:",
            repo_context_use_active: "현재 탭으로 전환",
            reset_section: "섹션 초기화",
            reset_all: "전체 초기화",
            reset_title: "설정을 초기화할까요?",
            reset_cancel: "취소",
            reset_confirm: "초기화",
            export_json: "JSON 내보내기",
            import_json: "JSON 가져오기",
        },
        UiLanguage::Japanese => ChromeLabels {
            close: "閉じる",
            search_hint: "設定を検索",
            search_empty: "一致する設定セクションがありません。",
            changed_badge: "変更済み",
            repo_context: "リポジトリ対象",
            repo_context_none: "開いているリポジトリなし",
            repo_context_active: "現在のタブ:",
            repo_context_bound: "この設定が対象にしているリポジトリ:",
            repo_context_use_active: "現在のタブに切替",
            reset_section: "セクションをリセット",
            reset_all: "すべてリセット",
            reset_title: "設定をリセットしますか？",
            reset_cancel: "キャンセル",
            reset_confirm: "リセット",
            export_json: "JSON を書き出し",
            import_json: "JSON を読み込み",
        },
        UiLanguage::Chinese => ChromeLabels {
            close: "关闭",
            search_hint: "搜索设置",
            search_empty: "没有匹配的设置分区。",
            changed_badge: "已更改",
            repo_context: "仓库上下文",
            repo_context_none: "没有打开的仓库",
            repo_context_active: "当前标签页:",
            repo_context_bound: "此窗口正在编辑的仓库:",
            repo_context_use_active: "切换到当前标签页",
            reset_section: "重置分区",
            reset_all: "全部重置",
            reset_title: "要重置这些设置吗？",
            reset_cancel: "取消",
            reset_confirm: "重置",
            export_json: "导出 JSON",
            import_json: "导入 JSON",
        },
        UiLanguage::French => ChromeLabels {
            close: "Fermer",
            search_hint: "Rechercher un réglage",
            search_empty: "Aucune section de réglages ne correspond.",
            changed_badge: "Modifié",
            repo_context: "Contexte dépôt",
            repo_context_none: "Aucun dépôt ouvert",
            repo_context_active: "Onglet actif :",
            repo_context_bound: "Dépôt ciblé par cette fenêtre :",
            repo_context_use_active: "Basculer vers l'onglet actif",
            reset_section: "Réinitialiser la section",
            reset_all: "Tout réinitialiser",
            reset_title: "Réinitialiser ces réglages ?",
            reset_cancel: "Annuler",
            reset_confirm: "Réinitialiser",
            export_json: "Exporter JSON",
            import_json: "Importer JSON",
        },
        UiLanguage::Spanish => ChromeLabels {
            close: "Cerrar",
            search_hint: "Buscar ajustes",
            search_empty: "No hay secciones que coincidan.",
            changed_badge: "Cambiado",
            repo_context: "Contexto del repositorio",
            repo_context_none: "No hay repos abiertos",
            repo_context_active: "Pestaña activa:",
            repo_context_bound: "Repositorio que edita esta ventana:",
            repo_context_use_active: "Cambiar a la pestaña activa",
            reset_section: "Restablecer sección",
            reset_all: "Restablecer todo",
            reset_title: "¿Restablecer estos ajustes?",
            reset_cancel: "Cancelar",
            reset_confirm: "Restablecer",
            export_json: "Exportar JSON",
            import_json: "Importar JSON",
        },
        _ => ChromeLabels {
            close: "Close",
            search_hint: "Search settings",
            search_empty: "No settings sections match this search.",
            changed_badge: "Changed",
            repo_context: "Repo context",
            repo_context_none: "No open repos",
            repo_context_active: "Active tab:",
            repo_context_bound: "This window is editing:",
            repo_context_use_active: "Switch to active tab",
            reset_section: "Reset section",
            reset_all: "Reset all",
            reset_title: "Reset settings?",
            reset_cancel: "Cancel",
            reset_confirm: "Reset",
            export_json: "Export JSON",
            import_json: "Import JSON",
        },
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
    let View::Workspace(_) = &mut app.view else {
        bail!("no repository is currently open");
    };
    let ws = app
        .workspace_by_path_mut(&target)
        .ok_or_else(|| anyhow!("repository tab is no longer open: {}", target.display()))?;
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
            rename_to: String::new(),
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

fn render_reset_actions(ui: &mut egui::Ui, app: &mut MergeFoxApp, labels: &ChromeLabels) {
    if ui.button(labels.export_json).clicked() {
        if let Err(err) = export_config_json(app) {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::err(format!("{err:#}")));
            }
        }
    }
    if ui.button(labels.import_json).clicked() {
        if let Err(err) = import_config_json(app, ui.ctx()) {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::err(format!("{err:#}")));
            }
        }
    }
    let Some(section) = app.settings_modal.as_ref().map(|modal| modal.section) else {
        return;
    };
    if section_reset_available(section) {
        if ui.button(labels.reset_section).clicked() {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.reset_scope = Some(SettingsResetScope::Section(section));
            }
        }
    }
    if ui.button(labels.reset_all).clicked() {
        if let Some(modal) = app.settings_modal.as_mut() {
            modal.reset_scope = Some(SettingsResetScope::Global);
        }
    }
}

fn show_reset_confirm(ctx: &egui::Context, app: &mut MergeFoxApp, labels: &ChromeLabels) {
    let Some(scope) = app
        .settings_modal
        .as_ref()
        .and_then(|modal| modal.reset_scope)
    else {
        return;
    };

    let mut close = false;
    let mut confirm = false;
    egui::Window::new(labels.reset_title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .default_width(420.0)
        .show(ctx, |ui| {
            ui.label(reset_body(scope, app));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(labels.reset_cancel).clicked() {
                    close = true;
                }
                if ui
                    .button(RichText::new(labels.reset_confirm).color(Color32::LIGHT_RED))
                    .clicked()
                {
                    confirm = true;
                }
            });
        });

    if confirm {
        apply_reset(ctx, app, scope);
        close = true;
    }
    if close {
        if let Some(modal) = app.settings_modal.as_mut() {
            modal.reset_scope = None;
        }
    }
}

fn reset_body(scope: SettingsResetScope, app: &MergeFoxApp) -> String {
    match scope {
        SettingsResetScope::Section(SettingsSection::General) => "Language, theme, and clone defaults will return to mergeFox defaults. Git identity is left untouched.".to_string(),
        SettingsResetScope::Section(SettingsSection::Repository) => "This repository's saved default remote, pull strategy, and pinned provider account will be cleared. Remotes and worktrees stay as-is.".to_string(),
        SettingsResetScope::Section(SettingsSection::Integrations) => "All connected provider accounts and saved PAT/OAuth tokens will be removed from mergeFox. Repository remotes are not changed.".to_string(),
        SettingsResetScope::Section(SettingsSection::Ai) => "The saved AI endpoint and its stored API key will be removed. The draft will return to the local Ollama default.".to_string(),
        SettingsResetScope::Section(SettingsSection::About) => "There is nothing to reset in About & Diagnostics.".to_string(),
        SettingsResetScope::Global => {
            let repo_note = app
                .settings_modal
                .as_ref()
                .and_then(|modal| modal.repo_path.as_ref())
                .map(|path| format!(" The current repo override for `{}` will also be cleared.", path.display()))
                .unwrap_or_default();
            format!(
                "All mergeFox settings will be reset: language, theme, clone defaults, AI endpoint, connected provider accounts, recents, and saved per-repo preferences.{repo_note} Git remotes and git identity are left untouched."
            )
        }
    }
}

fn export_config_json(app: &mut MergeFoxApp) -> anyhow::Result<()> {
    let Some(path) = rfd::FileDialog::new()
        .set_file_name("mergefox-settings.json")
        .add_filter("JSON", &["json"])
        .save_file()
    else {
        return Ok(());
    };
    let json = serde_json::to_string_pretty(&app.config)?;
    fs::write(&path, json)?;
    if let Some(modal) = app.settings_modal.as_mut() {
        modal.feedback = Some(Feedback::ok(format!(
            "Exported settings to {}",
            path.display()
        )));
    }
    app.hud = Some(crate::app::Hud::new(
        format!("Exported settings to {}", path.display()),
        1800,
    ));
    Ok(())
}

fn import_config_json(app: &mut MergeFoxApp, ctx: &egui::Context) -> anyhow::Result<()> {
    let Some(path) = rfd::FileDialog::new()
        .add_filter("JSON", &["json"])
        .pick_file()
    else {
        return Ok(());
    };
    let bytes = fs::read(&path)?;
    let imported: Config = serde_json::from_slice(&bytes)?;
    app.config = imported;
    app.config.save()?;
    sync_modal_from_config(app);
    crate::ui::fonts::ensure_language_fonts(ctx, app.config.ui_language);
    crate::ui::theme::apply(ctx, &app.config.theme);
    if let Some(modal) = app.settings_modal.as_mut() {
        modal.feedback = Some(Feedback::ok(format!(
            "Imported settings from {}",
            path.display()
        )));
    }
    app.hud = Some(crate::app::Hud::new(
        format!("Imported settings from {}", path.display()),
        1800,
    ));
    Ok(())
}

fn apply_reset(ctx: &egui::Context, app: &mut MergeFoxApp, scope: SettingsResetScope) {
    let result = match scope {
        SettingsResetScope::Section(SettingsSection::General) => reset_general(app, ctx),
        SettingsResetScope::Section(SettingsSection::Repository) => reset_repository(app),
        SettingsResetScope::Section(SettingsSection::Integrations) => reset_integrations(app),
        SettingsResetScope::Section(SettingsSection::Ai) => reset_ai(app),
        SettingsResetScope::Section(SettingsSection::About) => {
            Ok("Nothing to reset in About".to_string())
        }
        SettingsResetScope::Global => reset_all(app, ctx),
    };

    match result {
        Ok(message) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::ok(message.clone()));
            }
            app.hud = Some(crate::app::Hud::new(message, 1800));
        }
        Err(err) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::err(format!("{err:#}")));
            }
        }
    }
}

fn reset_general(app: &mut MergeFoxApp, ctx: &egui::Context) -> anyhow::Result<String> {
    app.config.ui_language = UiLanguage::default();
    app.config.theme = ThemeSettings::default();
    app.config.clone_defaults = CloneDefaults::default();
    if let Some(modal) = app.settings_modal.as_mut() {
        modal.language = app.config.ui_language;
        modal.theme = app.config.theme.clone();
    }
    crate::ui::fonts::ensure_language_fonts(ctx, app.config.ui_language);
    crate::ui::theme::apply(ctx, &app.config.theme);
    app.config.save()?;
    Ok("Reset General settings".to_string())
}

fn reset_repository(app: &mut MergeFoxApp) -> anyhow::Result<String> {
    let repo_path = app
        .settings_modal
        .as_ref()
        .and_then(|modal| modal.repo_path.clone())
        .ok_or_else(|| anyhow::anyhow!("no repository is attached to this settings window"))?;
    app.config
        .set_repo_settings(&repo_path, RepoSettings::default());
    if let Some(modal) = app.settings_modal.as_mut() {
        modal.default_remote = None;
        modal.pull_strategy = Default::default();
        modal.provider_account_slug = None;
    }
    app.config.save()?;
    Ok("Reset repository settings".to_string())
}

fn reset_integrations(app: &mut MergeFoxApp) -> anyhow::Result<String> {
    delete_saved_provider_credentials(app)?;
    app.config.provider_accounts.clear();
    for repo in app.config.repo_settings.values_mut() {
        repo.provider_account = None;
    }
    let integrations = integrations::IntegrationsDraft::from_config(&app.config);
    if let Some(modal) = app.settings_modal.as_mut() {
        modal.integrations = integrations;
        modal.provider_account_slug = None;
    }
    app.config.save()?;
    Ok("Reset integrations and removed saved credentials".to_string())
}

fn reset_ai(app: &mut MergeFoxApp) -> anyhow::Result<String> {
    delete_saved_ai_secret(app)?;
    app.config.ai_endpoint = None;
    let ai_draft = ai::AiDraft::from_config(&Config::default(), &app.secret_store);
    if let Some(modal) = app.settings_modal.as_mut() {
        modal.ai = ai_draft;
    }
    app.config.save()?;
    Ok("Reset AI settings".to_string())
}

fn reset_all(app: &mut MergeFoxApp, ctx: &egui::Context) -> anyhow::Result<String> {
    delete_saved_ai_secret(app)?;
    delete_saved_provider_credentials(app)?;
    app.config = Config::default();
    sync_modal_from_config(app);
    crate::ui::fonts::ensure_language_fonts(ctx, app.config.ui_language);
    crate::ui::theme::apply(ctx, &app.config.theme);
    app.config.save()?;
    Ok("Reset all mergeFox settings".to_string())
}

fn delete_saved_ai_secret(app: &mut MergeFoxApp) -> anyhow::Result<()> {
    if let Some(endpoint) = app.config.ai_endpoint.as_ref() {
        app.secret_store.delete_api_key(&endpoint.name)?;
    }
    Ok(())
}

fn delete_saved_provider_credentials(app: &mut MergeFoxApp) -> anyhow::Result<()> {
    for account in &app.config.provider_accounts {
        app.secret_store.delete_pat(&account.id)?;
    }
    Ok(())
}

fn sync_modal_from_config(app: &mut MergeFoxApp) {
    let integrations = integrations::IntegrationsDraft::from_config(&app.config);
    let ai_draft = ai::AiDraft::from_config(&app.config, &app.secret_store);
    let (repo_path, window_size) = match app.settings_modal.as_ref() {
        Some(modal) => (modal.repo_path.clone(), modal.window_size.clone()),
        None => return,
    };
    let repo_settings = repo_path
        .as_ref()
        .map(|path| app.config.repo_settings_for(path))
        .unwrap_or_default();
    let remotes = repo_path
        .as_ref()
        .and_then(|path| {
            app.workspace_by_path(path)
                .map(|ws| ws.repo.list_remotes().ok().unwrap_or_default())
        })
        .unwrap_or_default();
    if let Some(modal) = app.settings_modal.as_mut() {
        modal.language = app.config.ui_language;
        modal.theme = app.config.theme.clone();
        modal.repo_path = repo_path;
        modal.default_remote = repo_settings.default_remote;
        modal.pull_strategy = repo_settings.pull_strategy;
        modal.remotes = remotes
            .into_iter()
            .map(|remote| crate::app::RemoteDraft {
                name: remote.name,
                fetch_url: remote.fetch_url.unwrap_or_default(),
                push_url: remote.push_url.unwrap_or_default(),
                rename_to: String::new(),
            })
            .collect();
        modal.new_remote_name.clear();
        modal.new_fetch_url.clear();
        modal.new_push_url.clear();
        modal.integrations = integrations;
        modal.ai = ai_draft;
        modal.provider_account_slug = repo_settings.provider_account;
        modal.identity_name.clear();
        modal.identity_email.clear();
        modal.identity_global = false;
        modal.identity_loaded = false;
        modal.worktrees = None;
        modal.window_size = window_size;
        modal.reset_scope = None;
    }
}

fn open_repo_paths(app: &MergeFoxApp) -> Vec<PathBuf> {
    let crate::app::View::Workspace(tabs) = &app.view else {
        return Vec::new();
    };
    tabs.tabs
        .iter()
        .map(|ws| ws.repo.path().to_path_buf())
        .collect()
}

fn active_repo_path(app: &MergeFoxApp) -> Option<PathBuf> {
    let crate::app::View::Workspace(tabs) = &app.view else {
        return None;
    };
    if tabs.launcher_active {
        None
    } else {
        Some(tabs.current().repo.path().to_path_buf())
    }
}

fn repo_choice_label(path: Option<&Path>, labels: &ChromeLabels) -> String {
    let Some(path) = path else {
        return labels.repo_context_none.to_string();
    };
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string());
    format!("{leaf}  ({})", path.display())
}

fn switch_settings_repo(app: &mut MergeFoxApp, repo_path: Option<PathBuf>) {
    let repo_settings = repo_path
        .as_ref()
        .map(|path| app.config.repo_settings_for(path))
        .unwrap_or_default();
    let remotes = repo_path
        .as_ref()
        .and_then(|path| {
            app.workspace_by_path(path)
                .map(|ws| ws.repo.list_remotes().ok().unwrap_or_default())
        })
        .unwrap_or_default();

    if let Some(modal) = app.settings_modal.as_mut() {
        modal.repo_path = repo_path;
        modal.default_remote = repo_settings.default_remote;
        modal.pull_strategy = repo_settings.pull_strategy;
        modal.remotes = remotes
            .into_iter()
            .map(|remote| crate::app::RemoteDraft {
                name: remote.name,
                fetch_url: remote.fetch_url.unwrap_or_default(),
                push_url: remote.push_url.unwrap_or_default(),
                rename_to: String::new(),
            })
            .collect();
        modal.new_remote_name.clear();
        modal.new_fetch_url.clear();
        modal.new_push_url.clear();
        modal.provider_account_slug = repo_settings.provider_account;
        modal.identity_name.clear();
        modal.identity_email.clear();
        modal.identity_global = false;
        modal.identity_loaded = false;
        modal.worktrees = None;
        modal.feedback = None;
    }
}

fn persist_window_size(app: &mut MergeFoxApp) {
    let Some(size) = app
        .settings_modal
        .as_ref()
        .and_then(|modal| modal.window_size.clone())
    else {
        return;
    };
    let normalized = SettingsWindowState {
        width: size.width.clamp(320.0, 2400.0),
        height: size.height.clamp(240.0, 1800.0),
    };
    if (app.config.settings_window.width - normalized.width).abs() < 0.5
        && (app.config.settings_window.height - normalized.height).abs() < 0.5
    {
        return;
    }
    app.config.settings_window = normalized;
    let _ = app.config.save();
}

fn section_reset_available(section: SettingsSection) -> bool {
    !matches!(section, SettingsSection::About)
}

fn section_is_changed(app: &MergeFoxApp, section: SettingsSection) -> bool {
    match section {
        SettingsSection::General => {
            app.config.ui_language != UiLanguage::default()
                || app.config.theme != ThemeSettings::default()
                || app.config.clone_defaults != CloneDefaults::default()
        }
        SettingsSection::Repository => app
            .settings_modal
            .as_ref()
            .and_then(|modal| modal.repo_path.as_ref())
            .map(|path| app.config.repo_settings_for(path) != RepoSettings::default())
            .unwrap_or(false),
        SettingsSection::Integrations => !app.config.provider_accounts.is_empty(),
        SettingsSection::Ai => app.config.ai_endpoint.is_some(),
        SettingsSection::About => false,
    }
}

fn matching_sections(app: &MergeFoxApp, language: UiLanguage) -> Vec<SettingsSection> {
    let query = app
        .settings_modal
        .as_ref()
        .map(|modal| modal.search_query.trim().to_string())
        .unwrap_or_default();
    SettingsSection::all()
        .iter()
        .copied()
        .filter(|section| section_matches(*section, language, &query))
        .collect()
}

fn section_matches(section: SettingsSection, language: UiLanguage, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let mut searchable = section.label(language).to_lowercase();
    for keyword in section.search_keywords() {
        searchable.push(' ');
        searchable.push_str(keyword);
    }
    fuzzy_match(&searchable, query)
}

fn fuzzy_match(haystack: &str, query: &str) -> bool {
    let haystack = haystack.to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|needle| fuzzy_token_match(&haystack, needle))
}

fn fuzzy_token_match(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut chars = needle.chars();
    let mut current = chars.next();
    for c in haystack.chars() {
        if Some(c) == current {
            current = chars.next();
            if current.is_none() {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{fuzzy_match, section_matches, SettingsSection};
    use crate::config::UiLanguage;

    #[test]
    fn fuzzy_match_accepts_subsequence_tokens() {
        assert!(fuzzy_match("default remote pull strategy", "dr ps"));
        assert!(fuzzy_match("about diagnostics logs", "adl"));
        assert!(!fuzzy_match("general theme", "repo"));
    }

    #[test]
    fn section_search_uses_keywords() {
        assert!(section_matches(
            SettingsSection::Ai,
            UiLanguage::English,
            "ollama"
        ));
        assert!(section_matches(
            SettingsSection::Repository,
            UiLanguage::English,
            "worktree"
        ));
        assert!(!section_matches(
            SettingsSection::About,
            UiLanguage::English,
            "oauth"
        ));
    }
}
