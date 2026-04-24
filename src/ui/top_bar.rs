//! Top bar — only shown while a workspace is open.

use crate::app::{default_remote_name, tracked_upstream_for_branch, MergeFoxApp, View};
use crate::config::UiLanguage;

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    let View::Workspace(tabs) = &app.view else {
        return;
    };
    let ws = tabs.current();
    let path = ws.repo.path().display().to_string();
    let head = ws.repo.head_name();
    let last_error = app.last_error.clone();
    let forge_ready = ws.forge.repo.is_some();
    let forge_loading = ws.forge.loading;
    let has_active_job = ws.active_job.is_some();
    let git_available = app.git_capability.is_available();
    let git_capability_summary = app.git_capability.summary();

    // 진행률 데이터 추출 - 프로그레스바 표시용
    let active_job_progress = ws.active_job.as_ref().map(|j| {
        let p = j.snapshot();
        let fraction = if p.total > 0 {
            p.current as f32 / p.total as f32
        } else {
            0.0
        };
        let pct = if p.total > 0 {
            (p.current * 100) / p.total.max(1)
        } else {
            0
        };
        (j.label(), p.stage, fraction, pct)
    });
    let nav_task_progress = ws.nav_task.as_ref().map(|t| {
        let elapsed = t.started_at.elapsed().as_secs();
        // nav_task는 완료 시간을 알 수 없으므로 인디터미네이트 스타일 사용 (0.0)
        (t.label.clone(), elapsed, 0.0_f32)
    });

    // Tracking info for HEAD: upstream name + ahead/behind counts.
    // Cached on `repo_ui_cache` to avoid per-frame subprocess calls.
    let upstream_info = ws.repo_ui_cache.as_ref().and_then(|c| {
        c.branches
            .iter()
            .find(|b| b.is_head && !b.is_remote)
            .and_then(|b| b.upstream.as_ref().map(|u| (b.name.clone(), u.clone())))
    });
    let (ahead, behind) = ws
        .repo_ui_cache
        .as_ref()
        .map(|c| (c.ahead, c.behind))
        .unwrap_or((0, 0));
    let tracking_error = ws
        .repo_ui_cache
        .as_ref()
        .and_then(|c| c.tracking_error.clone());

    let mut go_home = false;
    let mut start_fetch: Option<String> = None;
    let mut start_push: Option<(String, bool)> = None; // (branch, force)
    let mut start_pull: Option<(String, String, crate::git::PullStrategy)> = None;
    let mut cancel_active_job = false;
    let mut open_reflog = false;
    let mut open_settings = false;
    let mut open_pr = false;
    let mut open_issue = false;
    let mut refresh_forge = false;
    let labels = top_bar_labels(app.config.ui_language);

    let cached_remotes: Vec<String> = ws
        .repo_ui_cache
        .as_ref()
        .map(|c| c.remotes.clone())
        .unwrap_or_default();
    let repo_settings = app.config.repo_settings_for(ws.repo.path());
    let default_remote = default_remote_name(ws, &app.config);
    let head_upstream = head
        .as_deref()
        .and_then(|head_name| tracked_upstream_for_branch(ws, head_name));
    let has_upstream = head_upstream.is_some();

    egui::TopBottomPanel::top("top_bar")
        .frame(crate::ui::chrome::toolbar_frame(&app.config.theme))
        .show(ctx, |ui| {
        crate::ui::chrome::apply_toolbar_visuals(ui, &app.config.theme);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            if crate::ui::chrome::toolbar_button(ui, "← Home").clicked() {
                go_home = true;
            }
            crate::ui::chrome::muted_pill(ui, format!("📁 {path}"));

            // ---- HEAD branch + tracking status ----
            if let Some(head_name) = &head {
                crate::ui::chrome::pill(
                    ui,
                    format!("🜲 {head_name}"),
                    crate::ui::theme::accent(&app.config.theme),
                );
                // Upstream tracking indicator
                if let Some((_branch, upstream_ref)) = &upstream_info {
                    crate::ui::chrome::muted_pill(ui, format!("→ {upstream_ref}"));
                    // Behind stays as a passive branch-state badge. Ahead is
                    // folded into the Push button below so the next action is
                    // visually tied to the unpushed commit count.
                    if behind > 0 {
                        crate::ui::chrome::pill(
                            ui,
                            format!("↓{behind}"),
                            egui::Color32::from_rgb(220, 160, 80),
                        )
                        .on_hover_text(format!(
                            "{behind} commit{} behind {upstream_ref} — pull to integrate",
                            if behind == 1 { "" } else { "s" }
                        ));
                    }
                    if ahead == 0 && behind == 0 {
                        crate::ui::chrome::pill(
                            ui,
                            "✓ synced",
                            egui::Color32::from_rgb(120, 190, 130),
                        )
                        .on_hover_text("Up to date with remote");
                    }
                } else if !cached_remotes.is_empty() {
                    crate::ui::chrome::pill(
                        ui,
                        "no upstream",
                        egui::Color32::from_rgb(200, 150, 80),
                    );
                }
            }

            // ---- Fetch / Push / Pull buttons with dropdown options ----
            ui.separator();
            let git_btn_size = egui::vec2(68.0, 24.0);
            ui.add_enabled_ui(!has_active_job && git_available, |ui| {
                // ── Fetch ──
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    if ui
                        .add_sized(git_btn_size, egui::Button::new(format!("⟳ {}", labels.fetch)))
                        .on_hover_text(format!("{} ({})", labels.fetch_hint, default_remote))
                        .clicked()
                    {
                        start_fetch = Some(default_remote.clone());
                    }
                    ui.menu_button(egui::RichText::new("⏷").size(10.0), |ui| {
                        for r in &cached_remotes {
                            if ui.button(format!("Fetch '{r}'")).clicked() {
                                start_fetch = Some(r.clone());
                                ui.close_menu();
                            }
                        }
                        ui.separator();
                        if ui.button("Fetch all remotes").clicked() {
                            start_fetch = Some("--all".to_string());
                            ui.close_menu();
                        }
                    });
                });

                // ── Pull ──
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    if ui
                        .add_enabled(
                            has_upstream,
                            egui::Button::new(format!("↓ {}", labels.pull)).min_size(git_btn_size),
                        )
                        .on_hover_text(if has_upstream {
                            labels.pull_hint
                        } else {
                            "Set an upstream first (right-click branch → Set upstream)"
                        })
                        .clicked()
                    {
                        if let Some((remote, branch)) = head_upstream.as_ref() {
                            start_pull = Some((
                                remote.clone(),
                                branch.clone(),
                                repo_settings.pull_strategy.to_git(),
                            ));
                        }
                    }
                    ui.add_enabled_ui(has_upstream, |ui| {
                        ui.menu_button(egui::RichText::new("⏷").size(10.0), |ui| {
                            if let Some((remote, branch)) = head_upstream.as_ref() {
                                if ui.button("Pull (merge)").clicked() {
                                    start_pull = Some((
                                        remote.clone(),
                                        branch.clone(),
                                        crate::git::PullStrategy::Merge,
                                    ));
                                    ui.close_menu();
                                }
                                if ui.button("Pull (rebase)").clicked() {
                                    start_pull = Some((
                                        remote.clone(),
                                        branch.clone(),
                                        crate::git::PullStrategy::Rebase,
                                    ));
                                    ui.close_menu();
                                }
                                if ui.button("Pull (fast-forward only)").clicked() {
                                    start_pull = Some((
                                        remote.clone(),
                                        branch.clone(),
                                        crate::git::PullStrategy::FastForwardOnly,
                                    ));
                                    ui.close_menu();
                                }
                            }
                        });
                    });
                });

                // ── Push ──
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let push_label = if ahead > 0 {
                        format!("↑{ahead} {}", labels.push)
                    } else {
                        format!("↑ {}", labels.push)
                    };
                    let push_btn_width = if ahead > 0 {
                        78.0 + (ahead.to_string().len() as f32 * 7.0)
                    } else {
                        git_btn_size.x
                    };
                    let push_hint = if ahead > 0 {
                        format!(
                            "{}\n\n{ahead} unpushed commit{} on this branch.",
                            labels.push_hint,
                            if ahead == 1 { "" } else { "s" }
                        )
                    } else {
                        labels.push_hint.to_string()
                    };
                    if ui
                        .add_sized(
                            egui::vec2(push_btn_width, git_btn_size.y),
                            egui::Button::new(push_label),
                        )
                        .on_hover_text(push_hint)
                        .clicked()
                    {
                        if let Some(h) = head.as_ref() {
                            start_push = Some((h.clone(), false));
                        }
                    }
                    ui.menu_button(egui::RichText::new("⏷").size(10.0), |ui| {
                        if let Some(h) = head.as_ref() {
                            if ui.button("Push").clicked() {
                                start_push = Some((h.clone(), false));
                                ui.close_menu();
                            }
                            if ui
                                .button(
                                    egui::RichText::new("Force push")
                                        .color(egui::Color32::from_rgb(230, 100, 100)),
                                )
                                .on_hover_text(
                                    "⚠ Overwrites the remote branch. Use after amend / rebase / reset.",
                                )
                                .clicked()
                            {
                                start_push = Some((h.clone(), true));
                                ui.close_menu();
                            }
                        }
                    });
                });
            });
            if !git_available {
                ui.separator();
                ui.colored_label(
                    egui::Color32::from_rgb(230, 180, 90),
                    "Degraded mode: install system git to enable fetch/pull/push, commit, stash, and rebase.",
                )
                .on_hover_text(git_capability_summary.clone());
            } else if let Some(err) = tracking_error.as_ref() {
                ui.separator();
                ui.colored_label(egui::Color32::from_rgb(230, 180, 90), "Tracking unavailable")
                    .on_hover_text(err);
            }

            // ---- Forge buttons ----
            ui.separator();
            ui.add_enabled_ui(forge_ready, |ui| {
                if ui
                    .button(format!("⇄ {}", labels.pull_request))
                    .on_hover_text(labels.pull_request_hint)
                    .clicked()
                {
                    open_pr = true;
                }
                if ui
                    .button(format!("• {}", labels.issue))
                    .on_hover_text(labels.issue_hint)
                    .clicked()
                {
                    open_issue = true;
                }
                if ui
                    .add_enabled(
                        !forge_loading,
                        egui::Button::new(format!("↻ {}", labels.forge_refresh)),
                    )
                    .on_hover_text(labels.forge_refresh_hint)
                    .clicked()
                {
                    refresh_forge = true;
                }
            });
            if ui
                .button(format!("↺ {}", labels.reflog))
                .on_hover_text(format!("{} (⌘⇧R)", labels.reflog_hint))
                .clicked()
            {
                open_reflog = true;
            }
            if ui
                .button(format!("⚙ {}", labels.settings))
                .on_hover_text(labels.settings_hint)
                .clicked()
            {
                open_settings = true;
            }

            // ---- 프로그레스바 표시 (스피너 대신 선형 프로그레스바) ----
            if let Some((label, stage, fraction, pct)) = active_job_progress {
                ui.separator();
                // 상단바 공간이 제한적이므로 컴팩트한 형태로 표시
                let text = if pct > 0 {
                    format!("{label} — {stage} ({pct}%)")
                } else {
                    format!("{label} — {stage}")
                };
                ui.weak(&text);
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .desired_width(120.0)
                        .show_percentage()
                        .animate(true),
                );
                if ui.small_button("Cancel").clicked() {
                    cancel_active_job = true;
                }
            }
            if let Some((label, elapsed, fraction)) = nav_task_progress {
                ui.separator();
                let text = format!("{label} ({elapsed}s)");
                ui.weak(&text);
                // nav_task는 진행률을 알 수 없으므로 인디터미네이트 스타일 (animate만)
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .desired_width(80.0)
                        .animate(true),
                );
            }

            // Errors used to render inline here as a red label, but
            // long multi-line diagnostics (push rejections, merge
            // conflicts) overflowed the top bar and pushed other
            // controls off-screen. They now go to the bottom-right
            // toast stack — see `ui::notifications`. The `last_error`
            // field is still populated for diagnostics / journal.
            let _ = last_error;
        });
    });

    if go_home {
        app.go_home();
    }
    if cancel_active_job {
        app.cancel_active_job();
    }
    if let Some(remote) = start_fetch {
        if remote == "--all" {
            // Fetch all remotes — kick one job per remote. Only the
            // first is tracked in `active_job`; the rest race in
            // parallel (git subprocess contention is minimal for fetch
            // since each targets a different remote).
            for r in &cached_remotes {
                app.start_fetch(r);
            }
        } else {
            app.start_fetch(&remote);
        }
    }
    if let Some((branch, force)) = start_push {
        if force {
            // Force push goes through confirmation dialog first, with a
            // pre-flight summary of how many commits on the remote are
            // about to be overwritten (the exact thing force push
            // silently destroys when someone else pushed while you were
            // working).
            let preflight = match &app.view {
                crate::app::View::Workspace(tabs) => Some(crate::preflight::force_push(
                    tabs.current().repo.path(),
                    &default_remote,
                    &branch,
                )),
                _ => None,
            };
            app.pending_prompt = Some(crate::ui::prompt::force_push_confirm(
                default_remote.clone(),
                branch,
                preflight,
            ));
        } else {
            app.start_push(&default_remote, &branch, false);
        }
    }
    if let Some((remote, branch, strategy)) = start_pull {
        app.start_pull(&remote, &branch, strategy);
    }
    if open_pr {
        app.open_pull_request_modal();
    }
    if open_issue {
        app.open_issue_modal();
    }
    if refresh_forge {
        app.refresh_active_forge();
    }
    if open_reflog {
        app.reflog_open = true;
    }
    if open_settings {
        app.open_settings();
    }
}

struct TopBarLabels {
    fetch: &'static str,
    fetch_hint: &'static str,
    pull: &'static str,
    pull_hint: &'static str,
    push: &'static str,
    push_hint: &'static str,
    pull_request: &'static str,
    pull_request_hint: &'static str,
    issue: &'static str,
    issue_hint: &'static str,
    forge_refresh: &'static str,
    forge_refresh_hint: &'static str,
    reflog: &'static str,
    reflog_hint: &'static str,
    settings: &'static str,
    settings_hint: &'static str,
}

fn top_bar_labels(language: UiLanguage) -> TopBarLabels {
    match language.resolved() {
        UiLanguage::Korean => TopBarLabels {
            fetch: "가져오기",
            fetch_hint: "기본 원격 저장소에서 fetch",
            pull: "풀",
            pull_hint: "원격 변경사항을 가져와 현재 브랜치에 통합합니다",
            push: "푸시",
            push_hint: "로컬 커밋을 원격에 올립니다",
            pull_request: "PR",
            pull_request_hint: "현재 브랜치에서 풀 리퀘스트를 만듭니다",
            issue: "이슈",
            issue_hint: "현재 저장소에 새 이슈를 만듭니다",
            forge_refresh: "PR/이슈 새로고침",
            forge_refresh_hint: "원격 PR과 이슈 목록을 다시 불러옵니다",
            reflog: "리플로그",
            reflog_hint: "최근 HEAD 이동 기록을 보고 안전하게 복구 브랜치를 만듭니다",
            settings: "설정",
            settings_hint: "언어, 기본 원격 저장소, pull 전략, remote URL을 관리합니다",
        },
        UiLanguage::Japanese => TopBarLabels {
            fetch: "取得",
            fetch_hint: "既定のリモートを fetch します",
            pull: "プル",
            pull_hint: "リモートの変更を取得して現在のブランチに統合します",
            push: "プッシュ",
            push_hint: "ローカルコミットをリモートに送信します",
            pull_request: "PR",
            pull_request_hint: "現在のブランチから Pull Request を作成します",
            issue: "Issue",
            issue_hint: "このリポジトリに新しい issue を作成します",
            forge_refresh: "PR/Issue 更新",
            forge_refresh_hint: "リモートの PR と issue 一覧を更新します",
            reflog: "Reflog",
            reflog_hint: "最近の HEAD 移動を見て安全な復旧ブランチを作成します",
            settings: "設定",
            settings_hint: "言語、既定のリモート、pull 戦略、remote URL を管理します",
        },
        UiLanguage::Chinese => TopBarLabels {
            fetch: "获取",
            fetch_hint: "从默认远端执行 fetch",
            pull: "拉取",
            pull_hint: "获取远端变更并集成到当前分支",
            push: "推送",
            push_hint: "将本地提交推送到远端",
            pull_request: "PR",
            pull_request_hint: "基于当前分支创建拉取请求",
            issue: "Issue",
            issue_hint: "为当前仓库创建新 issue",
            forge_refresh: "刷新 PR/Issue",
            forge_refresh_hint: "重新加载远端 PR 和 issue 列表",
            reflog: "Reflog",
            reflog_hint: "查看最近的 HEAD 变动，并在安全分支上恢复",
            settings: "设置",
            settings_hint: "管理语言、默认远端、pull 策略和 remote URL",
        },
        UiLanguage::French => TopBarLabels {
            fetch: "Récupérer",
            fetch_hint: "Récupérer le dépôt distant par défaut",
            pull: "Tirer",
            pull_hint: "Récupérer les changements distants et les intégrer",
            push: "Pousser",
            push_hint: "Envoyer les commits locaux vers le dépôt distant",
            pull_request: "PR",
            pull_request_hint: "Créer une pull request depuis la branche courante",
            issue: "Ticket",
            issue_hint: "Créer un ticket pour ce dépôt",
            forge_refresh: "Rafraîchir PR/Tickets",
            forge_refresh_hint: "Recharger les listes distantes de PR et de tickets",
            reflog: "Reflog",
            reflog_hint:
                "Parcourir les déplacements récents de HEAD et récupérer sur une branche sûre",
            settings: "Paramètres",
            settings_hint:
                "Gérer la langue, le remote par défaut, la stratégie de pull et les URL distantes",
        },
        UiLanguage::Spanish => TopBarLabels {
            fetch: "Obtener",
            fetch_hint: "Hacer fetch del remoto predeterminado",
            pull: "Tirar",
            pull_hint: "Obtener los cambios remotos e integrarlos",
            push: "Empujar",
            push_hint: "Enviar los commits locales al remoto",
            pull_request: "PR",
            pull_request_hint: "Crear un pull request desde la rama actual",
            issue: "Issue",
            issue_hint: "Crear un issue para este repositorio",
            forge_refresh: "Recargar PR/Issues",
            forge_refresh_hint: "Volver a cargar la lista remota de PR e issues",
            reflog: "Reflog",
            reflog_hint: "Revisar movimientos recientes de HEAD y recuperar en una rama segura",
            settings: "Ajustes",
            settings_hint:
                "Gestionar idioma, remoto predeterminado, estrategia de pull y URLs remotas",
        },
        _ => TopBarLabels {
            fetch: "Fetch",
            fetch_hint: "Fetch the default remote",
            pull: "Pull",
            pull_hint: "Fetch and integrate remote changes into the current branch",
            push: "Push",
            push_hint: "Upload local commits to the remote",
            pull_request: "PR",
            pull_request_hint: "Create a pull request from the current branch",
            issue: "Issue",
            issue_hint: "Create a new issue for this repository",
            forge_refresh: "Refresh PRs/Issues",
            forge_refresh_hint: "Reload the remote pull request and issue lists",
            reflog: "Reflog",
            reflog_hint: "Browse recent HEAD moves and recover on a safe branch",
            settings: "Settings",
            settings_hint: "Manage language, default remote, pull strategy, and remote URLs",
        },
    }
}
