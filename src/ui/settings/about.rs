use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use super::Feedback;
use crate::app::{Hud, MergeFoxApp, View};
use crate::config::UiLanguage;

pub fn show(ui: &mut egui::Ui, app: &mut MergeFoxApp) {
    let lang = app
        .settings_modal
        .as_ref()
        .map(|modal| modal.language.resolved())
        .unwrap_or_else(|| app.config.ui_language.resolved());
    let labels = labels(lang);
    let snapshot = DiagnosticsSnapshot::collect(app);

    ui.heading(labels.heading);
    ui.weak(labels.subtitle);
    ui.add_space(8.0);

    ui.group(|ui| {
        info_row(ui, labels.version, &snapshot.version);
        info_row(ui, labels.build_commit, &snapshot.build_commit);
        info_row(ui, labels.git, &snapshot.git_summary);
        info_row(ui, labels.platform, &snapshot.platform);
        info_row(
            ui,
            labels.ai_endpoint,
            if snapshot.ai_configured {
                labels.ai_configured
            } else {
                labels.ai_not_configured
            },
        );
        info_row(
            ui,
            labels.providers,
            &format!("{}", snapshot.provider_accounts),
        );
        info_row_optional(
            ui,
            labels.repo,
            snapshot.repo_path.as_ref().map(display_path),
            labels.none,
        );
        info_row_optional(
            ui,
            labels.config_path,
            snapshot.config_path.as_ref().map(display_path),
            labels.unavailable,
        );
        info_row_optional(
            ui,
            labels.log_path,
            snapshot.log_dir.as_ref().map(display_path),
            labels.unavailable,
        );
    });

    ui.add_space(10.0);
    ui.horizontal_wrapped(|ui| {
        if ui.button(labels.copy_diagnostics).clicked() {
            ui.ctx().copy_text(diagnostics_text(&snapshot));
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::ok(labels.copied));
            }
            app.hud = Some(Hud::new(labels.copied, 1600));
        }

        if ui.button(labels.open_config_folder).clicked() {
            let result = snapshot
                .config_path
                .as_ref()
                .and_then(|path| path.parent())
                .context(labels.no_config_folder)
                .and_then(open_in_file_manager);
            apply_open_result(app, result, labels.opened_config_folder);
        }

        if ui.button(labels.open_log_folder).clicked() {
            let result = snapshot
                .log_dir
                .as_deref()
                .context(labels.no_log_folder)
                .and_then(open_in_file_manager);
            apply_open_result(app, result, labels.opened_log_folder);
        }
    });
}

fn apply_open_result(app: &mut MergeFoxApp, result: Result<()>, ok_msg: &str) {
    match result {
        Ok(()) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::ok(ok_msg));
            }
            app.hud = Some(Hud::new(ok_msg, 1600));
        }
        Err(err) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::err(format!("{err:#}")));
            }
        }
    }
}

fn open_in_file_manager(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    let status = Command::new("open")
        .arg(path)
        .status()
        .context("launch `open`")?;

    #[cfg(target_os = "windows")]
    let status = Command::new("explorer")
        .arg(path)
        .status()
        .context("launch `explorer`")?;

    #[cfg(all(unix, not(target_os = "macos")))]
    let status = Command::new("xdg-open")
        .arg(path)
        .status()
        .context("launch `xdg-open`")?;

    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("file manager exited with status {status}");
    }
}

fn display_path(path: &PathBuf) -> String {
    path.display().to_string()
}

fn info_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).strong());
        ui.monospace(value);
    });
}

fn info_row_optional(ui: &mut egui::Ui, label: &str, value: Option<String>, fallback: &str) {
    info_row(ui, label, value.as_deref().unwrap_or(fallback));
}

#[derive(Debug, Clone)]
struct DiagnosticsSnapshot {
    version: String,
    build_commit: String,
    git_summary: String,
    platform: String,
    config_path: Option<PathBuf>,
    log_dir: Option<PathBuf>,
    repo_path: Option<PathBuf>,
    ai_configured: bool,
    provider_accounts: usize,
}

impl DiagnosticsSnapshot {
    fn collect(app: &MergeFoxApp) -> Self {
        let repo_path = match &app.view {
            View::Workspace(tabs) if !tabs.launcher_active => {
                Some(tabs.current().repo.path().to_path_buf())
            }
            _ => None,
        };
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            build_commit: option_env!("MERGEFOX_BUILD_COMMIT")
                .unwrap_or("unknown")
                .to_string(),
            git_summary: app.git_capability.summary(),
            platform: format!("{} / {}", std::env::consts::OS, std::env::consts::ARCH),
            config_path: crate::config::config_path(),
            log_dir: crate::logging::log_dir(),
            repo_path,
            ai_configured: app.config.ai_endpoint.is_some(),
            provider_accounts: app.config.provider_accounts.len(),
        }
    }
}

fn diagnostics_text(snapshot: &DiagnosticsSnapshot) -> String {
    let repo = snapshot
        .repo_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "none".to_string());
    let config = snapshot
        .config_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "unavailable".to_string());
    let logs = snapshot
        .log_dir
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "unavailable".to_string());
    format!(
        "mergeFox diagnostics\n\
         version: {}\n\
         build_commit: {}\n\
         git: {}\n\
         platform: {}\n\
         active_repo: {}\n\
         config_path: {}\n\
         log_dir: {}\n\
         ai_endpoint_configured: {}\n\
         provider_accounts: {}",
        snapshot.version,
        snapshot.build_commit,
        snapshot.git_summary,
        snapshot.platform,
        repo,
        config,
        logs,
        snapshot.ai_configured,
        snapshot.provider_accounts,
    )
}

struct Labels {
    heading: &'static str,
    subtitle: &'static str,
    version: &'static str,
    build_commit: &'static str,
    git: &'static str,
    platform: &'static str,
    repo: &'static str,
    config_path: &'static str,
    log_path: &'static str,
    ai_endpoint: &'static str,
    ai_configured: &'static str,
    ai_not_configured: &'static str,
    providers: &'static str,
    none: &'static str,
    unavailable: &'static str,
    copy_diagnostics: &'static str,
    copied: &'static str,
    open_config_folder: &'static str,
    opened_config_folder: &'static str,
    open_log_folder: &'static str,
    opened_log_folder: &'static str,
    no_config_folder: &'static str,
    no_log_folder: &'static str,
}

fn labels(lang: UiLanguage) -> Labels {
    match lang {
        UiLanguage::Korean => Labels {
            heading: "정보 및 진단",
            subtitle: "버그 리포트와 환경 확인에 필요한 핵심 정보를 모았습니다.",
            version: "버전",
            build_commit: "빌드 커밋",
            git: "Git",
            platform: "플랫폼",
            repo: "현재 저장소",
            config_path: "설정 파일",
            log_path: "로그 폴더",
            ai_endpoint: "AI 엔드포인트",
            ai_configured: "설정됨",
            ai_not_configured: "설정 안 됨",
            providers: "연결된 Provider 수",
            none: "없음",
            unavailable: "사용 불가",
            copy_diagnostics: "진단 정보 복사",
            copied: "진단 정보를 복사했습니다",
            open_config_folder: "설정 폴더 열기",
            opened_config_folder: "설정 폴더를 열었습니다",
            open_log_folder: "로그 폴더 열기",
            opened_log_folder: "로그 폴더를 열었습니다",
            no_config_folder: "설정 폴더를 찾을 수 없습니다",
            no_log_folder: "로그 폴더를 찾을 수 없습니다",
        },
        _ => Labels {
            heading: "About & Diagnostics",
            subtitle: "Quick environment details for bug reports and support.",
            version: "Version",
            build_commit: "Build commit",
            git: "Git",
            platform: "Platform",
            repo: "Active repo",
            config_path: "Config file",
            log_path: "Log folder",
            ai_endpoint: "AI endpoint",
            ai_configured: "Configured",
            ai_not_configured: "Not configured",
            providers: "Connected providers",
            none: "None",
            unavailable: "Unavailable",
            copy_diagnostics: "Copy diagnostics",
            copied: "Copied diagnostics",
            open_config_folder: "Open config folder",
            opened_config_folder: "Opened config folder",
            open_log_folder: "Open log folder",
            opened_log_folder: "Opened log folder",
            no_config_folder: "Config folder is unavailable",
            no_log_folder: "Log folder is unavailable",
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{diagnostics_text, DiagnosticsSnapshot};

    #[test]
    fn diagnostics_text_includes_core_fields() {
        let snapshot = DiagnosticsSnapshot {
            version: "0.1.0".to_string(),
            build_commit: "abc123".to_string(),
            git_summary: "System git available".to_string(),
            platform: "macos / aarch64".to_string(),
            config_path: Some(PathBuf::from("/tmp/config.json")),
            log_dir: Some(PathBuf::from("/tmp/logs")),
            repo_path: Some(PathBuf::from("/tmp/repo")),
            ai_configured: true,
            provider_accounts: 2,
        };

        let text = diagnostics_text(&snapshot);
        assert!(text.contains("version: 0.1.0"));
        assert!(text.contains("build_commit: abc123"));
        assert!(text.contains("active_repo: /tmp/repo"));
        assert!(text.contains("provider_accounts: 2"));
    }

    #[test]
    fn diagnostics_text_uses_fallbacks_for_missing_paths() {
        let snapshot = DiagnosticsSnapshot {
            version: "0.1.0".to_string(),
            build_commit: "unknown".to_string(),
            git_summary: "missing".to_string(),
            platform: "linux / x86_64".to_string(),
            config_path: None,
            log_dir: None,
            repo_path: None,
            ai_configured: false,
            provider_accounts: 0,
        };

        let text = diagnostics_text(&snapshot);
        assert!(text.contains("active_repo: none"));
        assert!(text.contains("config_path: unavailable"));
        assert!(text.contains("log_dir: unavailable"));
    }
}
