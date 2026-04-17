//! "Repository" settings section — default remote, pull strategy, and
//! remote URL CRUD for the currently active tab.
//!
//! All fields save immediately:
//!
//! * Changing the default-remote combo or the pull-strategy combo writes
//!   the per-repo config entry.
//! * Editing a remote's URLs uses an explicit "Save URLs" button because
//!   saving on every keystroke would spam gix with malformed URLs
//!   during typing.
//! * Add / Delete write immediately.

use anyhow::bail;
use egui::{Color32, ComboBox, RichText, TextEdit};

use super::{finish_repo_update, persist_config, with_settings_repo, Feedback};
use crate::app::MergeFoxApp;
use crate::config::{PullStrategyPref, RepoSettings, UiLanguage};

pub fn show(ui: &mut egui::Ui, app: &mut MergeFoxApp) {
    let language = current_language(app);
    let labels = labels(language);

    ui.heading(labels.heading);
    ui.separator();

    // Render the "no repo open" empty-state and bail early. Nothing in
    // this section has meaning without an active workspace.
    let repo_path = app
        .settings_modal
        .as_ref()
        .and_then(|m| m.repo_path.clone());
    let Some(repo_path) = repo_path else {
        ui.weak(labels.no_repo_open);
        return;
    };

    ui.label(format!("{} {}", labels.current_repo, repo_path.display()));
    ui.add_space(8.0);

    let mut intent: Option<Intent> = None;

    {
        let Some(modal) = app.settings_modal.as_mut() else {
            return;
        };

        // --- default remote + pull strategy -----------------------------
        ui.horizontal(|ui| {
            ui.label(labels.default_remote);
            let before = modal.default_remote.clone();
            ComboBox::from_id_salt("settings_default_remote")
                .selected_text(
                    modal
                        .default_remote
                        .as_deref()
                        .unwrap_or(labels.auto_remote),
                )
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut modal.default_remote, None, labels.auto_remote);
                    for remote in &modal.remotes {
                        ui.selectable_value(
                            &mut modal.default_remote,
                            Some(remote.name.clone()),
                            &remote.name,
                        );
                    }
                });
            if modal.default_remote != before {
                intent = Some(Intent::SavePreferences);
            }

            ui.separator();

            ui.label(labels.pull_strategy);
            let before_strategy = modal.pull_strategy;
            ComboBox::from_id_salt("settings_pull_strategy")
                .selected_text(modal.pull_strategy.label())
                .show_ui(ui, |ui| {
                    for strategy in [
                        PullStrategyPref::Merge,
                        PullStrategyPref::Rebase,
                        PullStrategyPref::FastForwardOnly,
                    ] {
                        ui.selectable_value(&mut modal.pull_strategy, strategy, strategy.label());
                    }
                });
            if modal.pull_strategy != before_strategy {
                intent = Some(Intent::SavePreferences);
            }
        });
        ui.weak(labels.repo_settings_hint);

        // --- per-repo account selection --------------------------------
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(labels.provider_account);
            let accounts = &app.config.provider_accounts;
            // Current selection slug lives on `modal`; we mirror it from
            // RepoSettings on open and write back on change.
            let before_account = modal.provider_account_slug.clone();
            let display = modal
                .provider_account_slug
                .as_deref()
                .and_then(|slug| {
                    accounts
                        .iter()
                        .find(|a| a.id.slug() == slug)
                        .map(|a| format!("{} ({})", a.display_name, a.id.kind.slug()))
                })
                .unwrap_or_else(|| labels.auto_account.to_string());
            ComboBox::from_id_salt("settings_repo_account")
                .selected_text(display)
                .show_ui(ui, |ui| {
                    // "Auto" = detect from remote URL host.
                    if ui
                        .selectable_label(
                            modal.provider_account_slug.is_none(),
                            labels.auto_account,
                        )
                        .clicked()
                    {
                        modal.provider_account_slug = None;
                    }
                    ui.separator();
                    for acc in accounts {
                        let slug = acc.id.slug();
                        let selected =
                            modal.provider_account_slug.as_deref() == Some(slug.as_str());
                        let label = format!(
                            "{}  ({}, {})",
                            acc.display_name,
                            acc.id.kind.slug(),
                            acc.id.username
                        );
                        if ui.selectable_label(selected, label).clicked() {
                            modal.provider_account_slug = Some(slug);
                        }
                    }
                });
            if modal.provider_account_slug != before_account {
                intent = Some(Intent::SavePreferences);
            }
        });
        ui.weak(labels.provider_account_hint);

        ui.add_space(12.0);
        ui.heading(labels.remote_urls);
        ui.separator();
        if modal.remotes.is_empty() {
            ui.weak(labels.no_remotes);
        } else {
            for remote in &mut modal.remotes {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&remote.name).strong());
                        if modal.default_remote.as_deref() == Some(remote.name.as_str()) {
                            ui.label(RichText::new(labels.default_badge).weak());
                        }
                    });
                    ui.label(labels.fetch_url);
                    ui.add(
                        TextEdit::singleline(&mut remote.fetch_url).desired_width(f32::INFINITY),
                    );
                    ui.label(labels.push_url);
                    ui.add(
                        TextEdit::singleline(&mut remote.push_url)
                            .desired_width(f32::INFINITY)
                            .hint_text(labels.push_url_placeholder),
                    );
                    // Rename row — kept separate from URL edits because
                    // `git remote rename` also rewrites upstream tracking
                    // config, which is not what users expect from "Save
                    // URLs".
                    ui.horizontal(|ui| {
                        ui.label(labels.rename_to);
                        ui.add(
                            TextEdit::singleline(&mut remote.rename_to)
                                .desired_width(160.0)
                                .hint_text(&remote.name),
                        );
                        let new_name = remote.rename_to.trim().to_string();
                        let ok = !new_name.is_empty() && new_name != remote.name;
                        ui.add_enabled_ui(ok, |ui| {
                            if ui.button(labels.rename_remote).clicked() {
                                intent = Some(Intent::RenameRemote {
                                    old_name: remote.name.clone(),
                                    new_name,
                                });
                            }
                        });
                    });
                    ui.horizontal(|ui| {
                        if ui.button(labels.save_remote).clicked() {
                            intent = Some(Intent::SaveRemote {
                                name: remote.name.clone(),
                                fetch_url: remote.fetch_url.clone(),
                                push_url: remote.push_url.clone(),
                            });
                        }
                        if ui
                            .button(RichText::new(labels.delete_remote).color(Color32::LIGHT_RED))
                            .clicked()
                        {
                            intent = Some(Intent::DeleteRemote {
                                name: remote.name.clone(),
                            });
                        }
                    });
                });
                ui.add_space(6.0);
            }
        }

        ui.add_space(12.0);
        ui.heading(labels.add_remote);
        ui.separator();
        ui.label(labels.remote_name);
        ui.add(
            TextEdit::singleline(&mut modal.new_remote_name)
                .desired_width(f32::INFINITY)
                .hint_text("origin"),
        );
        ui.label(labels.fetch_url);
        ui.add(
            TextEdit::singleline(&mut modal.new_fetch_url)
                .desired_width(f32::INFINITY)
                .hint_text("https://example.com/owner/repo.git"),
        );
        ui.label(labels.push_url);
        ui.add(
            TextEdit::singleline(&mut modal.new_push_url)
                .desired_width(f32::INFINITY)
                .hint_text(labels.push_url_placeholder),
        );
        if ui.button(labels.add_remote_button).clicked() {
            intent = Some(Intent::AddRemote {
                name: modal.new_remote_name.clone(),
                fetch_url: modal.new_fetch_url.clone(),
                push_url: modal.new_push_url.clone(),
            });
        }

        // --- worktrees --------------------------------------------------
        //
        // Lazy-load on first render of this settings section: the list is
        // read-only state so we can just re-query on each open rather
        // than cache-invalidate it through the full ws cache pipeline.
        ui.add_space(16.0);
        ui.heading(labels.worktrees);
        ui.separator();
        if modal.worktrees.is_none() {
            intent = intent.or(Some(Intent::RefreshWorktrees));
        }
        match &modal.worktrees {
            Some(list) if list.is_empty() => {
                ui.weak(labels.no_worktrees);
            }
            Some(list) => {
                for wt in list {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(wt.path.display().to_string()).strong());
                            if wt.is_main {
                                ui.label(RichText::new(labels.main_badge).weak());
                            }
                            if wt.is_locked {
                                ui.label(RichText::new(labels.locked_badge).weak());
                            }
                            if wt.is_prunable {
                                ui.colored_label(
                                    Color32::from_rgb(240, 180, 96),
                                    labels.prunable_badge,
                                );
                            }
                        });
                        if let Some(branch) = &wt.branch {
                            ui.weak(format!("branch: {branch}"));
                        } else if wt.is_detached {
                            ui.weak("detached HEAD");
                        }
                        if !wt.is_main {
                            ui.horizontal(|ui| {
                                if ui
                                    .button(
                                        RichText::new(labels.remove_worktree)
                                            .color(Color32::LIGHT_RED),
                                    )
                                    .clicked()
                                {
                                    intent = Some(Intent::RemoveWorktree {
                                        path: wt.path.clone(),
                                        force: false,
                                    });
                                }
                                if ui.button(labels.remove_worktree_force).clicked() {
                                    intent = Some(Intent::RemoveWorktree {
                                        path: wt.path.clone(),
                                        force: true,
                                    });
                                }
                            });
                        }
                    });
                    ui.add_space(4.0);
                }
            }
            None => {
                ui.weak(labels.loading);
            }
        }
    }

    if let Some(intent) = intent {
        handle_intent(app, intent, &labels);
    }
}

enum Intent {
    SavePreferences,
    SaveRemote {
        name: String,
        fetch_url: String,
        push_url: String,
    },
    DeleteRemote {
        name: String,
    },
    AddRemote {
        name: String,
        fetch_url: String,
        push_url: String,
    },
    RenameRemote {
        old_name: String,
        new_name: String,
    },
    RefreshWorktrees,
    RemoveWorktree {
        path: std::path::PathBuf,
        force: bool,
    },
}

fn handle_intent(app: &mut MergeFoxApp, intent: Intent, labels: &Labels) {
    match intent {
        Intent::SavePreferences => save_preferences(app, labels),
        Intent::SaveRemote {
            name,
            fetch_url,
            push_url,
        } => update_remote(app, &name, &fetch_url, &push_url, labels),
        Intent::DeleteRemote { name } => delete_remote(app, &name, labels),
        Intent::AddRemote {
            name,
            fetch_url,
            push_url,
        } => add_remote(app, &name, &fetch_url, &push_url, labels),
        Intent::RenameRemote { old_name, new_name } => {
            rename_remote(app, &old_name, &new_name, labels)
        }
        Intent::RefreshWorktrees => refresh_worktrees(app),
        Intent::RemoveWorktree { path, force } => remove_worktree(app, &path, force, labels),
    }
}

fn refresh_worktrees(app: &mut MergeFoxApp) {
    let result = with_settings_repo(app, |repo| repo.list_worktrees());
    if let Some(modal) = app.settings_modal.as_mut() {
        match result {
            Ok(list) => modal.worktrees = Some(list),
            Err(_) => modal.worktrees = Some(Vec::new()),
        }
    }
}

fn remove_worktree(app: &mut MergeFoxApp, path: &std::path::Path, force: bool, labels: &Labels) {
    let path = path.to_path_buf();
    let result = with_settings_repo(app, |repo| repo.remove_worktree(&path, force));
    match result {
        Ok(()) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::ok(labels.removed_worktree.to_string()));
                modal.worktrees = None; // force reload
            }
            app.hud = Some(crate::app::Hud::new(labels.removed_worktree, 1600));
        }
        Err(err) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::err(format!("{err:#}")));
            }
        }
    }
}

fn rename_remote(app: &mut MergeFoxApp, old_name: &str, new_name: &str, labels: &Labels) {
    let result = with_settings_repo(app, |repo| {
        let trimmed = new_name.trim();
        if trimmed.is_empty() {
            bail!("new remote name cannot be empty");
        }
        if trimmed.contains(char::is_whitespace) {
            bail!("remote names cannot contain whitespace");
        }
        repo.rename_remote(old_name, trimmed)
    });
    // If the default-remote pointed at the old name, migrate it so the
    // rename doesn't silently break push/pull for this repo.
    if result.is_ok() {
        if let Some(modal) = app.settings_modal.as_mut() {
            if modal.default_remote.as_deref() == Some(old_name) {
                modal.default_remote = Some(new_name.trim().to_string());
            }
            // Persist the provider_account / default_remote change.
        }
        save_preferences(app, labels);
    }
    finish_repo_update(app, result, labels.renamed_remote);
}

fn save_preferences(app: &mut MergeFoxApp, labels: &Labels) {
    let (repo_path, settings) = {
        let Some(modal) = app.settings_modal.as_ref() else {
            return;
        };
        let Some(repo_path) = modal.repo_path.clone() else {
            return;
        };
        let settings = RepoSettings {
            default_remote: modal.default_remote.clone(),
            pull_strategy: modal.pull_strategy,
            provider_account: modal.provider_account_slug.clone(),
        };
        (repo_path, settings)
    };

    app.config.set_repo_settings(&repo_path, settings);
    persist_config(app, labels.saved_prefs);
}

fn update_remote(
    app: &mut MergeFoxApp,
    name: &str,
    fetch_url: &str,
    push_url: &str,
    labels: &Labels,
) {
    let result = with_settings_repo(app, |repo| {
        let fetch_url = fetch_url.trim();
        if fetch_url.is_empty() {
            bail!("fetch URL cannot be empty");
        }
        let push_url = cleaned(push_url);
        repo.update_remote_urls(name, fetch_url, push_url.as_deref())
    });
    finish_repo_update(app, result, labels.updated_remote);
}

fn delete_remote(app: &mut MergeFoxApp, name: &str, labels: &Labels) {
    let result = with_settings_repo(app, |repo| repo.delete_remote(name));
    if result.is_ok() {
        if let Some(modal) = app.settings_modal.as_mut() {
            if modal.default_remote.as_deref() == Some(name) {
                modal.default_remote = None;
            }
        }
    }
    finish_repo_update(app, result, labels.deleted_remote);
}

fn add_remote(app: &mut MergeFoxApp, name: &str, fetch_url: &str, push_url: &str, labels: &Labels) {
    let result = with_settings_repo(app, |repo| {
        let name = name.trim();
        let fetch_url = fetch_url.trim();
        if name.is_empty() {
            bail!("remote name cannot be empty");
        }
        if fetch_url.is_empty() {
            bail!("fetch URL cannot be empty");
        }
        let push_url = cleaned(push_url);
        repo.add_remote(name, fetch_url, push_url.as_deref())
    });

    match result {
        Ok(()) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                if modal.default_remote.is_none() {
                    modal.default_remote = Some(name.trim().to_string());
                }
                modal.new_remote_name.clear();
                modal.new_fetch_url.clear();
                modal.new_push_url.clear();
            }
            finish_repo_update(app, Ok(()), labels.added_remote);
        }
        Err(err) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::err(format!("{err:#}")));
            }
        }
    }
}

fn cleaned(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn current_language(app: &MergeFoxApp) -> UiLanguage {
    app.settings_modal
        .as_ref()
        .map(|m| m.language.resolved())
        .unwrap_or_else(|| app.config.ui_language.resolved())
}

struct Labels {
    heading: &'static str,
    no_repo_open: &'static str,
    current_repo: &'static str,
    default_remote: &'static str,
    auto_remote: &'static str,
    pull_strategy: &'static str,
    repo_settings_hint: &'static str,
    remote_urls: &'static str,
    no_remotes: &'static str,
    fetch_url: &'static str,
    push_url: &'static str,
    push_url_placeholder: &'static str,
    save_remote: &'static str,
    delete_remote: &'static str,
    default_badge: &'static str,
    add_remote: &'static str,
    remote_name: &'static str,
    add_remote_button: &'static str,
    provider_account: &'static str,
    provider_account_hint: &'static str,
    auto_account: &'static str,
    saved_prefs: &'static str,
    updated_remote: &'static str,
    deleted_remote: &'static str,
    added_remote: &'static str,
    rename_to: &'static str,
    rename_remote: &'static str,
    renamed_remote: &'static str,
    worktrees: &'static str,
    no_worktrees: &'static str,
    main_badge: &'static str,
    locked_badge: &'static str,
    prunable_badge: &'static str,
    remove_worktree: &'static str,
    remove_worktree_force: &'static str,
    removed_worktree: &'static str,
    loading: &'static str,
}

fn labels(lang: UiLanguage) -> Labels {
    match lang {
        UiLanguage::Korean => Labels {
            heading: "저장소 설정",
            no_repo_open:
                "저장소를 열면 기본 원격, Pull 전략, 원격 URL 편집 같은 저장소별 설정을 관리할 수 있습니다.",
            current_repo: "대상 저장소:",
            default_remote: "기본 원격 저장소",
            auto_remote: "(자동 선택)",
            pull_strategy: "기본 Pull 전략",
            repo_settings_hint:
                "여기서 저장한 기본 원격과 Pull 전략은 fetch/pull/push 동작의 기본값으로 사용됩니다.",
            remote_urls: "원격 저장소 URL",
            no_remotes: "등록된 원격이 없습니다.\n아래 \"원격 저장소 추가\" 폼에서 fetch/push URL을 넣거나, 웰컴 화면에서 Publish 플로우를 사용하세요.",
            fetch_url: "Fetch URL",
            push_url: "Push URL",
            push_url_placeholder: "비워두면 Fetch URL을 사용합니다",
            save_remote: "URL 저장",
            delete_remote: "삭제",
            default_badge: "기본값",
            add_remote: "원격 저장소 추가",
            remote_name: "원격 이름",
            add_remote_button: "추가",
            provider_account: "Push/Pull 계정",
            provider_account_hint: "이 저장소에서 push/pull/fetch 시 사용할 계정입니다. 여러 GitHub 계정이 있을 때 유용합니다.",
            auto_account: "(원격 URL에서 자동 감지)",
            saved_prefs: "저장소 기본값을 저장했습니다",
            updated_remote: "원격 URL을 업데이트했습니다",
            deleted_remote: "원격을 삭제했습니다",
            added_remote: "원격을 추가했습니다",
            rename_to: "새 이름",
            rename_remote: "이름 변경",
            renamed_remote: "원격 이름을 변경했습니다",
            worktrees: "워크트리",
            no_worktrees: "연결된 워크트리가 없습니다.\n워크트리는 같은 저장소에서 여러 브랜치를 동시에 체크아웃할 수 있게 해줍니다. 터미널에서 `git worktree add <경로> <브랜치>`로 추가할 수 있습니다.",
            main_badge: "메인",
            locked_badge: "잠금",
            prunable_badge: "정리 대상",
            remove_worktree: "삭제",
            remove_worktree_force: "강제 삭제",
            removed_worktree: "워크트리를 삭제했습니다",
            loading: "불러오는 중…",
        },
        _ => Labels {
            heading: "Repository Settings",
            no_repo_open:
                "Open a repository to manage per-repo settings like the default remote, pull strategy, and remote URLs.",
            current_repo: "Repository:",
            default_remote: "Default remote",
            auto_remote: "(auto-select)",
            pull_strategy: "Default pull strategy",
            repo_settings_hint:
                "The saved default remote and pull strategy are used as the default for fetch, pull, and push actions.",
            remote_urls: "Remote URLs",
            no_remotes: "No remotes are configured for this repository yet.\nAdd one in the \"Add Remote\" form below, or publish your current branch from the welcome screen.",
            fetch_url: "Fetch URL",
            push_url: "Push URL",
            push_url_placeholder: "Leave empty to use the fetch URL",
            save_remote: "Save URLs",
            delete_remote: "Delete",
            default_badge: "default",
            add_remote: "Add Remote",
            remote_name: "Remote name",
            add_remote_button: "Add",
            provider_account: "Push/Pull account",
            provider_account_hint: "Which connected account to use for push/pull/fetch on this repo. Useful when you have multiple GitHub accounts (personal + work).",
            auto_account: "(auto-detect from remote URL)",
            saved_prefs: "Saved repository defaults",
            updated_remote: "Updated remote URLs",
            deleted_remote: "Deleted remote",
            added_remote: "Added remote",
            rename_to: "Rename to",
            rename_remote: "Rename",
            renamed_remote: "Renamed remote",
            worktrees: "Worktrees",
            no_worktrees: "No linked worktrees.\nWorktrees let you have multiple branches checked out at once from the same repo. Add one from a terminal: `git worktree add <path> <branch>`.",
            main_badge: "main",
            locked_badge: "locked",
            prunable_badge: "prunable",
            remove_worktree: "Remove",
            remove_worktree_force: "Force remove",
            removed_worktree: "Removed worktree",
            loading: "Loading…",
        },
    }
}
