//! "General" settings section — language and fixed app theme preferences.
//!
//! We keep this section immediate-save: selecting a new language or changing
//! theme palette values applies to the app right away and persists to disk.

use egui::{ComboBox, RichText, Slider, Stroke};

use super::persist_config;
use crate::app::MergeFoxApp;
use crate::config::{ThemeColor, ThemePalette, ThemePreset, UiLanguage};

pub fn show(ui: &mut egui::Ui, app: &mut MergeFoxApp) {
    let language = current_language(app);
    let labels = labels(language);

    ui.heading(labels.heading);
    ui.separator();

    let mut new_language: Option<UiLanguage> = None;
    let theme_changed = {
        let Some(modal) = app.settings_modal.as_mut() else {
            return;
        };

        ui.horizontal(|ui| {
            ui.label(labels.language);
            ComboBox::from_id_salt("settings_language")
                .selected_text(modal.language.label())
                .show_ui(ui, |ui| {
                    for option in [
                        UiLanguage::System,
                        UiLanguage::English,
                        UiLanguage::Korean,
                        UiLanguage::Japanese,
                        UiLanguage::Chinese,
                        UiLanguage::French,
                        UiLanguage::Spanish,
                    ] {
                        let was = modal.language;
                        ui.selectable_value(&mut modal.language, option, option.label());
                        if modal.language != was {
                            new_language = Some(modal.language);
                        }
                    }
                });
        });
        ui.weak(labels.language_hint);

        ui.add_space(18.0);
        ui.heading(labels.theme_heading);
        ui.separator();
        ui.weak(labels.theme_hint);
        ui.add_space(10.0);

        let before_theme = modal.theme.clone();

        ui.horizontal_wrapped(|ui| {
            for preset in [
                ThemePreset::MergeFox,
                ThemePreset::Light,
                ThemePreset::Colorblind,
                ThemePreset::Custom,
            ] {
                theme_card(ui, preset, &mut modal.theme, &labels);
                ui.add_space(8.0);
            }
        });

        ui.add_space(12.0);

        let selected_preset = modal.theme.preset;
        let selected_palette = modal.theme.active_palette();
        let editable = selected_preset == ThemePreset::Custom;

        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!(
                        "{}: {}",
                        labels.palette_heading,
                        preset_label(selected_preset, &labels)
                    ))
                    .strong(),
                );
                if !editable
                    && ui
                        .button(labels.make_custom)
                        .on_hover_text(labels.make_custom_hint)
                        .clicked()
                {
                    modal.theme.set_custom_from(selected_preset);
                }
            });
            ui.add_space(6.0);

            if editable {
                ui.weak(labels.custom_hint);
            } else {
                ui.weak(labels.palette_readonly_hint);
            }

            ui.add_space(8.0);
            palette_row(
                ui,
                labels.accent,
                &mut modal.theme.custom_palette.accent,
                &selected_palette.accent,
                editable,
            );
            palette_row(
                ui,
                labels.background,
                &mut modal.theme.custom_palette.background,
                &selected_palette.background,
                editable,
            );
            palette_row(
                ui,
                labels.foreground,
                &mut modal.theme.custom_palette.foreground,
                &selected_palette.foreground,
                editable,
            );

            ui.add_space(8.0);
            if editable {
                ui.checkbox(
                    &mut modal.theme.custom_palette.translucent_panels,
                    labels.translucent_panels,
                );
                ui.horizontal(|ui| {
                    ui.label(labels.contrast);
                    ui.add(Slider::new(
                        &mut modal.theme.custom_palette.contrast,
                        0..=100,
                    ));
                    ui.monospace(format!("{}", modal.theme.custom_palette.contrast));
                });

                ui.horizontal_wrapped(|ui| {
                    ui.weak(labels.custom_bases);
                    for base in [
                        ThemePreset::MergeFox,
                        ThemePreset::Light,
                        ThemePreset::Colorblind,
                    ] {
                        if ui.button(preset_label(base, &labels)).clicked() {
                            modal.theme.set_custom_from(base);
                        }
                    }
                });
            } else {
                bool_preview(
                    ui,
                    labels.translucent_panels,
                    selected_palette.translucent_panels,
                    &labels,
                );
                readonly_slider(ui, labels.contrast, selected_palette.contrast);
            }
        });

        modal.theme != before_theme
    };

    if let Some(lang) = new_language {
        crate::ui::fonts::ensure_language_fonts(ui.ctx(), lang);
        app.config.ui_language = lang;
        persist_config(app, labels.language_saved);
    }

    if theme_changed {
        let Some(theme) = app.settings_modal.as_ref().map(|modal| modal.theme.clone()) else {
            return;
        };
        crate::ui::theme::apply(ui.ctx(), &theme);
        app.config.theme = theme;
        persist_config(app, labels.theme_saved);
    }

    render_git_runtime(ui, app, &labels);
    render_git_identity(ui, app, &labels);
    render_clone_defaults(ui, app, &labels);
}

fn render_git_runtime(ui: &mut egui::Ui, app: &mut MergeFoxApp, labels: &Labels) {
    ui.add_space(18.0);
    ui.heading(labels.git_runtime_heading);
    ui.separator();
    ui.weak(labels.git_runtime_hint);
    ui.add_space(8.0);

    ui.horizontal(|ui| {
        let (text, color) = if app.git_capability.is_available() {
            (
                app.git_capability.summary(),
                egui::Color32::from_rgb(120, 200, 140),
            )
        } else {
            (
                app.git_missing_message("MergeFox"),
                egui::Color32::from_rgb(230, 180, 90),
            )
        };
        ui.colored_label(color, text);
        if ui.button(labels.git_runtime_refresh).clicked() {
            app.refresh_git_capability();
        }
    });

    if !app.git_capability.is_available() {
        ui.weak(app.git_capability.install_guidance());
    }

    ui.add_space(10.0);
    egui::CollapsingHeader::new(labels.git_runtime_log)
        .default_open(false)
        .show(ui, |ui| {
            let entries = crate::git::recent_git_log();
            if entries.is_empty() {
                ui.weak(labels.git_runtime_log_empty);
                return;
            }
            for entry in entries.iter().rev().take(12) {
                let status = if entry.exit_code == 0 {
                    "ok".to_string()
                } else {
                    format!("exit {}", entry.exit_code)
                };
                ui.monospace(format!(
                    "[t={} | {} ms | {}] git {}",
                    entry.timestamp,
                    entry.duration_ms,
                    status,
                    entry.args.join(" ")
                ));
                ui.weak(&entry.cwd);
                if !entry.stderr_head.is_empty() {
                    ui.weak(format!("stderr: {}", entry.stderr_head));
                }
                ui.add_space(6.0);
            }
        });
}

/// Git author identity subsection.
///
/// Shows the current `user.name` and `user.email` from git config (local
/// or global) in editable text fields. Changes are written back to the
/// active repo's local config; a "Set globally" checkbox lets the user
/// push it to `~/.gitconfig` instead.
///
/// The values are read ONCE when the settings window opens (via
/// `SettingsModal::identity_*` fields initialised in `mod.rs::open`),
/// and written on each change. We avoid re-reading every frame because
/// `git config` is a subprocess.
fn render_git_identity(ui: &mut egui::Ui, app: &mut MergeFoxApp, labels: &Labels) {
    ui.add_space(18.0);
    ui.heading(labels.identity_heading);
    ui.separator();
    ui.weak(labels.identity_hint);
    ui.add_space(8.0);

    let Some(modal) = app.settings_modal.as_mut() else {
        return;
    };

    // Lazy-init: read from git config on first render.
    if !modal.identity_loaded {
        if let Some(path) = modal.repo_path.as_ref() {
            modal.identity_name =
                crate::git::cli::run_line(path, ["config", "user.name"]).unwrap_or_default();
            modal.identity_email =
                crate::git::cli::run_line(path, ["config", "user.email"]).unwrap_or_default();
        } else {
            modal.identity_name = crate::git::cli::GitCommand::new(std::path::Path::new("."))
                .args(["config", "--global", "user.name"])
                .run()
                .map(|o| o.stdout_str().trim().to_string())
                .unwrap_or_default();
            modal.identity_email = crate::git::cli::GitCommand::new(std::path::Path::new("."))
                .args(["config", "--global", "user.email"])
                .run()
                .map(|o| o.stdout_str().trim().to_string())
                .unwrap_or_default();
        }
        modal.identity_loaded = true;
    }

    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(labels.identity_name);
        if ui
            .add(
                egui::TextEdit::singleline(&mut modal.identity_name)
                    .desired_width(260.0)
                    .hint_text("e.g. Jeoung myeoungho"),
            )
            .changed()
        {
            changed = true;
        }
    });
    ui.horizontal(|ui| {
        ui.label(labels.identity_email);
        if ui
            .add(
                egui::TextEdit::singleline(&mut modal.identity_email)
                    .desired_width(260.0)
                    .hint_text("e.g. user@example.com"),
            )
            .changed()
        {
            changed = true;
        }
    });

    ui.checkbox(&mut modal.identity_global, labels.identity_global);
    ui.weak(labels.identity_global_hint);

    if changed {
        let name = modal.identity_name.trim().to_string();
        let email = modal.identity_email.trim().to_string();
        let global = modal.identity_global;

        let result: anyhow::Result<()> = (|| {
            if global {
                if !name.is_empty() {
                    crate::git::cli::GitCommand::new(std::path::Path::new("."))
                        .args(["config", "--global", "user.name", &name])
                        .run()?;
                }
                if !email.is_empty() {
                    crate::git::cli::GitCommand::new(std::path::Path::new("."))
                        .args(["config", "--global", "user.email", &email])
                        .run()?;
                }
            } else if let Some(path) = modal.repo_path.as_ref() {
                if !name.is_empty() {
                    crate::git::cli::run(path, ["config", "user.name", &name])?;
                }
                if !email.is_empty() {
                    crate::git::cli::run(path, ["config", "user.email", &email])?;
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            app.set_git_error("Updating git identity", format!("{e:#}"));
        }
    }
}

/// Clone-flow defaults subsection.
///
/// Three knobs:
///   * **Size policy** — Ask / Always full / Always shallow
///   * **Prompt threshold (MB)** — only honoured under Ask policy
///   * **Shallow depth (commits)** — used by Shallow button + Always shallow
///
/// Immediate-save like the rest of General: changing any knob persists
/// right away. No "apply" button — same mental model as macOS System
/// Settings.
fn render_clone_defaults(ui: &mut egui::Ui, app: &mut MergeFoxApp, labels: &Labels) {
    use crate::config::{CloneDefaults, CloneSizePolicy};
    use egui::{ComboBox, DragValue};

    ui.add_space(18.0);
    ui.heading(labels.clone_heading);
    ui.separator();
    ui.weak(labels.clone_hint);
    ui.add_space(8.0);

    let mut changed_defaults: Option<CloneDefaults> = None;

    {
        let Some(modal) = app.settings_modal.as_mut() else {
            return;
        };
        let _ = modal; // reserved — future preview rendering could live here

        // Pull a mutable view of the live config so the UI reflects saves
        // immediately across sections (we don't stage a draft copy).
        let defaults = &mut app.config.clone_defaults;
        let before = defaults.clone();

        ui.horizontal(|ui| {
            ui.label(labels.clone_policy);
            ComboBox::from_id_salt("settings_clone_policy")
                .selected_text(policy_label(defaults.size_policy, labels))
                .show_ui(ui, |ui| {
                    for option in [
                        CloneSizePolicy::Prompt,
                        CloneSizePolicy::AlwaysFull,
                        CloneSizePolicy::AlwaysShallow,
                    ] {
                        ui.selectable_value(
                            &mut defaults.size_policy,
                            option,
                            policy_label(option, labels),
                        );
                    }
                });
        });

        ui.horizontal(|ui| {
            ui.label(labels.clone_threshold);
            ui.add_enabled_ui(defaults.size_policy == CloneSizePolicy::Prompt, |ui| {
                ui.add(
                    DragValue::new(&mut defaults.prompt_threshold_mb)
                        .range(10u32..=100_000u32)
                        .speed(25.0)
                        .suffix(" MB"),
                );
            });
            ui.weak(labels.clone_threshold_hint);
        });

        ui.horizontal(|ui| {
            ui.label(labels.clone_depth);
            ui.add(
                DragValue::new(&mut defaults.shallow_depth)
                    .range(1u32..=100_000u32)
                    .speed(5.0)
                    .suffix(labels.clone_depth_suffix),
            );
            ui.weak(labels.clone_depth_hint);
        });

        if *defaults != before {
            changed_defaults = Some(defaults.clone());
        }
    }

    if changed_defaults.is_some() {
        persist_config(app, labels.clone_saved);
    }
}

fn policy_label(p: crate::config::CloneSizePolicy, labels: &Labels) -> &'static str {
    use crate::config::CloneSizePolicy::*;
    match p {
        Prompt => labels.clone_policy_prompt,
        AlwaysFull => labels.clone_policy_full,
        AlwaysShallow => labels.clone_policy_shallow,
    }
}

fn theme_card(
    ui: &mut egui::Ui,
    preset: ThemePreset,
    theme: &mut crate::config::ThemeSettings,
    labels: &Labels,
) {
    let selected = theme.preset == preset;
    let palette = match preset {
        ThemePreset::Custom => theme.custom_palette.clone(),
        _ => preset_palette(preset),
    };
    let stroke = if selected {
        Stroke::new(1.5, ui.visuals().selection.bg_fill)
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke
    };
    let fill = if selected {
        ui.visuals().faint_bg_color
    } else {
        ui.visuals().widgets.inactive.bg_fill
    };

    let response = egui::Frame::group(ui.style())
        .stroke(stroke)
        .fill(fill)
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
        .show(ui, |ui| {
            ui.set_min_width(150.0);
            ui.label(RichText::new(preset_label(preset, labels)).strong());
            ui.small(preset_description(preset, labels));
            ui.add_space(6.0);
            swatch_strip(ui, &palette);
        })
        .response
        .interact(egui::Sense::click());

    if response.clicked() {
        theme.preset = preset;
    }
}

fn swatch_strip(ui: &mut egui::Ui, palette: &ThemePalette) {
    ui.horizontal(|ui| {
        for color in [palette.accent, palette.background, palette.foreground] {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(28.0, 18.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 5.0, color.to_color32());
            ui.painter().rect_stroke(
                rect,
                5.0,
                Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
            );
        }
    });
}

fn palette_row(
    ui: &mut egui::Ui,
    label: &str,
    custom_slot: &mut ThemeColor,
    preview: &ThemeColor,
    editable: bool,
) {
    ui.horizontal(|ui| {
        ui.add_sized([140.0, 20.0], egui::Label::new(label));
        if editable {
            let mut color = custom_slot.to_color32();
            if ui.color_edit_button_srgba(&mut color).changed() {
                *custom_slot = ThemeColor::from_color32(color);
            }
            ui.monospace(custom_slot.hex());
        } else {
            let mut color = preview.to_color32();
            ui.add_enabled_ui(false, |ui| {
                ui.color_edit_button_srgba(&mut color);
            });
            ui.monospace(preview.hex());
        }
    });
}

fn bool_preview(ui: &mut egui::Ui, label: &str, value: bool, labels: &Labels) {
    ui.horizontal(|ui| {
        ui.add_sized([140.0, 20.0], egui::Label::new(label));
        ui.weak(if value {
            labels.enabled
        } else {
            labels.disabled
        });
    });
}

fn readonly_slider(ui: &mut egui::Ui, label: &str, value: u8) {
    ui.horizontal(|ui| {
        ui.add_sized([140.0, 20.0], egui::Label::new(label));
        let mut preview = value;
        ui.add_enabled(false, Slider::new(&mut preview, 0..=100));
        ui.monospace(format!("{value}"));
    });
}

fn preset_palette(preset: ThemePreset) -> ThemePalette {
    match preset {
        ThemePreset::MergeFox => ThemePalette::mergefox(),
        ThemePreset::Light => ThemePalette::light(),
        ThemePreset::Dark => ThemePalette::mergefox(),
        ThemePreset::Colorblind => ThemePalette::colorblind(),
        ThemePreset::Custom => ThemePalette::default(),
    }
}

fn preset_label(preset: ThemePreset, labels: &Labels) -> &'static str {
    match preset {
        ThemePreset::MergeFox => labels.mergefox_theme,
        ThemePreset::Light => labels.light_theme,
        ThemePreset::Dark => labels.mergefox_theme,
        ThemePreset::Colorblind => labels.colorblind_theme,
        ThemePreset::Custom => labels.custom_theme,
    }
}

fn preset_description(preset: ThemePreset, labels: &Labels) -> &'static str {
    match preset {
        ThemePreset::MergeFox => labels.mergefox_desc,
        ThemePreset::Light => labels.light_desc,
        ThemePreset::Dark => labels.mergefox_desc,
        ThemePreset::Colorblind => labels.colorblind_desc,
        ThemePreset::Custom => labels.custom_desc,
    }
}

fn current_language(app: &MergeFoxApp) -> UiLanguage {
    app.settings_modal
        .as_ref()
        .map(|m| m.language.resolved())
        .unwrap_or_else(|| app.config.ui_language.resolved())
}

struct Labels {
    heading: &'static str,
    language: &'static str,
    language_hint: &'static str,
    language_saved: &'static str,
    theme_heading: &'static str,
    theme_hint: &'static str,
    theme_saved: &'static str,
    mergefox_theme: &'static str,
    light_theme: &'static str,
    colorblind_theme: &'static str,
    custom_theme: &'static str,
    mergefox_desc: &'static str,
    light_desc: &'static str,
    colorblind_desc: &'static str,
    custom_desc: &'static str,
    palette_heading: &'static str,
    palette_readonly_hint: &'static str,
    custom_hint: &'static str,
    make_custom: &'static str,
    make_custom_hint: &'static str,
    accent: &'static str,
    background: &'static str,
    foreground: &'static str,
    translucent_panels: &'static str,
    contrast: &'static str,
    custom_bases: &'static str,
    enabled: &'static str,
    disabled: &'static str,
    git_runtime_heading: &'static str,
    git_runtime_hint: &'static str,
    git_runtime_refresh: &'static str,
    git_runtime_log: &'static str,
    git_runtime_log_empty: &'static str,
    identity_heading: &'static str,
    identity_hint: &'static str,
    identity_name: &'static str,
    identity_email: &'static str,
    identity_global: &'static str,
    identity_global_hint: &'static str,
    clone_heading: &'static str,
    clone_hint: &'static str,
    clone_policy: &'static str,
    clone_policy_prompt: &'static str,
    clone_policy_full: &'static str,
    clone_policy_shallow: &'static str,
    clone_threshold: &'static str,
    clone_threshold_hint: &'static str,
    clone_depth: &'static str,
    clone_depth_suffix: &'static str,
    clone_depth_hint: &'static str,
    clone_saved: &'static str,
}

fn labels(lang: UiLanguage) -> Labels {
    match lang {
        UiLanguage::Korean => Labels {
            heading: "일반",
            language: "언어",
            language_hint:
                "현재는 설정 화면과 일부 상단 메뉴 라벨부터 언어 선호도를 반영합니다.",
            language_saved: "언어를 저장했습니다",
            theme_heading: "테마",
            theme_hint: "MergeFox는 시스템 다크/라이트 전환을 따라가지 않고, 여기서 선택한 테마를 고정해서 사용합니다.",
            theme_saved: "테마를 저장했습니다",
            mergefox_theme: "MergeFox",
            light_theme: "라이트",
            colorblind_theme: "색각 보조",
            custom_theme: "커스텀",
            mergefox_desc: "브랜드 기본 테마",
            light_desc: "밝고 차분한 작업용 테마",
            colorblind_desc: "색 의존도를 낮추고 명암 대비를 크게 둔 접근성 테마",
            custom_desc: "팔레트를 직접 조정하는 테마",
            palette_heading: "팔레트",
            palette_readonly_hint: "기본 테마 팔레트 미리보기입니다. 편집하려면 커스텀 테마로 복제하세요.",
            custom_hint: "커스텀 테마는 아래 팔레트와 대비 값을 바로 수정할 수 있습니다.",
            make_custom: "커스텀으로 복제",
            make_custom_hint: "현재 프리셋을 커스텀 테마의 시작점으로 복제합니다.",
            accent: "Accent",
            background: "Background",
            foreground: "Foreground",
            translucent_panels: "반투명 사이드바",
            contrast: "대비",
            custom_bases: "기본값으로 다시 시작:",
            enabled: "사용",
            disabled: "해제",
            git_runtime_heading: "System Git",
            git_runtime_hint:
                "MergeFox는 읽기 경로는 gix를 쓰고, 커밋/스태시/리베이스/네트워크 작업은 설치된 git CLI를 사용합니다.",
            git_runtime_refresh: "다시 확인",
            git_runtime_log: "최근 git 명령",
            git_runtime_log_empty: "아직 실행한 git 명령이 없습니다.",
            identity_heading: "Git 사용자 정보",
            identity_hint: "커밋할 때 기록되는 이름과 이메일입니다. 앱 간 공유되는 git config 값입니다.",
            identity_name: "이름",
            identity_email: "이메일",
            identity_global: "전역 설정 (~/.gitconfig)",
            identity_global_hint: "체크하면 모든 저장소에 적용됩니다. 해제하면 현재 저장소에만 적용됩니다.",
            clone_heading: "클론 기본값",
            clone_hint:
                "큰 저장소를 클론할 때의 기본 동작을 설정합니다. GitHub / GitLab 저장소는 클론 전에 크기를 미리 확인합니다.",
            clone_policy: "크기별 정책",
            clone_policy_prompt: "크면 물어보기",
            clone_policy_full: "항상 전체",
            clone_policy_shallow: "항상 shallow",
            clone_threshold: "프롬프트 임계값",
            clone_threshold_hint: "이 크기를 넘으면 shallow 여부를 묻습니다.",
            clone_depth: "Shallow 깊이",
            clone_depth_suffix: " 커밋",
            clone_depth_hint: "shallow 선택 시 받을 최근 커밋 수입니다.",
            clone_saved: "클론 기본값을 저장했습니다",
        },
        _ => Labels {
            heading: "General",
            language: "Language",
            language_hint:
                "The saved preference applies to this settings window and selected top-bar labels for now.",
            language_saved: "Saved language",
            theme_heading: "Theme",
            theme_hint:
                "MergeFox now uses a fixed app theme and does not follow system dark/light changes.",
            theme_saved: "Saved theme",
            mergefox_theme: "MergeFox",
            light_theme: "Light",
            colorblind_theme: "Colorblind",
            custom_theme: "Custom",
            mergefox_desc: "Brand-default palette",
            light_desc: "Quiet light workspace palette",
            colorblind_desc: "High-luminance contrast palette with non-color cues",
            custom_desc: "Editable palette with your own colors",
            palette_heading: "Palette",
            palette_readonly_hint:
                "This preset palette is read-only. Duplicate it into Custom to edit the colors.",
            custom_hint:
                "Custom theme updates immediately as you tweak the palette and contrast.",
            make_custom: "Customize",
            make_custom_hint: "Use this preset as the starting point for a custom theme.",
            accent: "Accent",
            background: "Background",
            foreground: "Foreground",
            translucent_panels: "Translucent sidebar",
            contrast: "Contrast",
            custom_bases: "Start from preset:",
            enabled: "Enabled",
            disabled: "Disabled",
            git_runtime_heading: "System Git",
            git_runtime_hint:
                "MergeFox uses gix for fast in-process reads, but commit/stash/rebase/network actions run through the installed git CLI.",
            git_runtime_refresh: "Refresh",
            git_runtime_log: "Recent git commands",
            git_runtime_log_empty: "No git commands have run yet.",
            identity_heading: "Git identity",
            identity_hint: "Name and email recorded in every commit you make. Shared across all apps that use git config.",
            identity_name: "Name",
            identity_email: "Email",
            identity_global: "Set globally (~/.gitconfig)",
            identity_global_hint: "When checked, applies to all repositories. Otherwise only the current repo.",
            clone_heading: "Clone defaults",
            clone_hint:
                "How mergeFox should behave when cloning large repositories. Supported for GitHub / GitLab hosted repos where we can check size beforehand.",
            clone_policy: "Size policy",
            clone_policy_prompt: "Ask when large",
            clone_policy_full: "Always full",
            clone_policy_shallow: "Always shallow",
            clone_threshold: "Prompt threshold",
            clone_threshold_hint: "Prompt for shallow above this size.",
            clone_depth: "Shallow depth",
            clone_depth_suffix: " commits",
            clone_depth_hint: "Recent commits kept when cloning shallow.",
            clone_saved: "Saved clone defaults",
        },
    }
}
