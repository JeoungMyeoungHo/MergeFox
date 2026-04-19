//! Welcome screen: unified input (URL / path / query) + Recent list.

use std::path::PathBuf;

use crate::app::{
    BlobLimitUnit, CloneSizePrompt, LargeRepoMode, LargeRepoModeState, MergeFoxApp,
};
use crate::clone::{self, CloneFilter, CloneOpts, Stage};
use crate::config::{CloneFilterPolicy, CloneSizePolicy, UiLanguage};
use crate::git_url;
use crate::providers::{
    self, AccountId, CreateRepositoryDraft, ProviderAccount, RemoteRepoOwner, RemoteRepoOwnerKind,
    RemoteRepoSummary,
};

/// What the user asked us to do while drawing this frame. We defer
/// execution until after the UI closure so we're not mutating `app`
/// while it's already borrowed.
#[derive(Default)]
struct Intent {
    open_path: Option<PathBuf>,
    /// User picked a folder via "Init" — we'll run `git init` there then
    /// open it as a fresh workspace. Accepts empty folders and also
    /// non-empty ones that aren't already a repo (git itself handles
    /// the "already a repo" case idempotently, so we just let it).
    init_path: Option<PathBuf>,
    /// "User hit Clone" — sent before we know the repo size. The
    /// welcome-level handler decides whether to preflight, prompt, or
    /// spawn the clone directly based on `CloneSizePolicy`.
    start_clone: Option<(String, PathBuf)>,
    /// Modal decisions coming out of `CloneSizePrompt`.
    clone_decision: Option<CloneDecision>,
    open_settings: bool,
    refresh_remote_repos: Option<AccountId>,
    load_remote_repo_owners: Option<AccountId>,
    create_remote_repo: Option<(AccountId, CreateRepositoryDraft)>,
    /// User resolved the post-init account picker. `Some(Some(slug))` =
    /// associate that account; `Some(None)` = skip; `None` = still
    /// waiting on the user this frame.
    init_pick_decision: Option<Option<String>>,
}

#[derive(Debug)]
enum CloneDecision {
    /// Spawn a full clone for the pending prompt, clear the prompt.
    Full,
    /// Spawn a shallow clone at the prompt's chosen depth.
    Shallow,
    /// Spawn a partial clone with `--filter=blob:none` — the "one-click
    /// recommended response" offered on the large-repo prompt. Fetches
    /// every commit/tree object but defers blob downloads, which is the
    /// single biggest saving for binary-heavy repos.
    PartialBlobNone,
    /// Dismiss the prompt without cloning.
    Cancel,
}

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    if app.active_welcome_state().is_none() {
        return;
    }

    let default_parent = app.default_clone_parent();
    let recents = app.config.recents.clone();
    let connected_accounts = app.repo_browser_accounts();
    let labels = labels(app.config.ui_language.resolved());
    // Snapshot the persisted clone defaults once per frame — they feed
    // the first-time seed of `LargeRepoModeState`. Read once to avoid
    // re-borrowing `app.config` while `state` holds a mutable borrow.
    let clone_defaults = app.config.clone_defaults.clone();
    let mut intent = Intent::default();

    egui::CentralPanel::default().show(ctx, |ui| {
        let Some(state) = app.active_welcome_state_mut() else {
            return;
        };

        let selected_missing = state
            .remote_repos
            .selected_account
            .as_ref()
            .map(|selected| {
                connected_accounts
                    .iter()
                    .all(|account| account.id != *selected)
            })
            .unwrap_or(true);
        if selected_missing {
            state.remote_repos.selected_account = connected_accounts.first().map(|a| a.id.clone());
            state.remote_repos.repos.clear();
            state.remote_repos.last_error = None;
            state.remote_repos.loaded_once = false;
            state.remote_repos.create_repo = crate::app::CreateRemoteRepoState::default();
        }
        if state.remote_repos.task.is_none() && !state.remote_repos.loaded_once {
            if let Some(account_id) = state.remote_repos.selected_account.clone() {
                intent.refresh_remote_repos = Some(account_id);
            }
        }

        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.heading("mergeFox");
            ui.label(egui::RichText::new(labels.tagline).weak());
            ui.add_space(24.0);
        });

        // ---------- unified input ----------
        let mut focus_me = None;
        ui.vertical_centered(|ui| {
            let input = egui::TextEdit::singleline(&mut state.input)
                .hint_text(labels.input_hint)
                .desired_width(560.0);
            let resp = ui.add(input);
            if !ctx.memory(|m| m.focused().is_some()) {
                focus_me = Some(resp.id);
            }

            ui.add_space(8.0);
            render_input_suggestion(ui, &state.input, &default_parent, &mut intent, &labels);
        });
        if let Some(id) = focus_me {
            ctx.memory_mut(|m| m.request_focus(id));
        }

        ui.add_space(16.0);
        ui.separator();

        // ---------- action buttons ----------
        ui.horizontal(|ui| {
            if ui.button(labels.open_local_folder).clicked() {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    intent.open_path = Some(path);
                }
            }
            if ui.button(labels.clone_from_url).clicked() {
                state.input.clear();
                state.input.push_str("https://");
            }
            if ui
                .button(labels.init_new_folder)
                .on_hover_text(labels.init_new_folder_hint)
                .clicked()
            {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    intent.init_path = Some(path);
                }
            }
            if ui.button(labels.settings).clicked() {
                intent.open_settings = true;
            }
        });

        ui.add_space(8.0);
        render_large_repo_mode_panel(ui, &mut state.large_repo_mode, &clone_defaults, &labels);

        ui.add_space(16.0);

        ui.columns(2, |columns| {
            render_recents(
                &mut columns[0],
                &recents,
                &state.input,
                &mut intent,
                &labels,
            );
            render_connected_repos(
                &mut columns[1],
                state,
                &default_parent,
                &connected_accounts,
                &mut intent,
                &labels,
            );
        });

        // ---------- clone progress ----------
        if state.clone_preflight.is_some() {
            ui.add_space(16.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(labels.checking_size);
            });
        }

        if let Some(prompt) = &state.clone_size_prompt {
            ui.add_space(16.0);
            ui.separator();
            render_clone_size_prompt(ui, prompt, &labels, &mut intent);
        }

        if let Some(handle) = &state.clone {
            ui.add_space(16.0);
            ui.separator();
            render_clone_progress(ui, handle);
        }

        if state.pending_init_pick.is_some() {
            ui.add_space(16.0);
            ui.separator();
            render_init_account_pick(ui, state, &connected_accounts, &mut intent, &labels);
        }

        // ---------- error banner ----------
        if let Some(err) = &app.last_error {
            ui.add_space(12.0);
            ui.colored_label(egui::Color32::LIGHT_RED, err);
        }
    });

    // Apply intent after the UI closure releases its borrow on `app.view`.
    if let Some((url, dest)) = intent.start_clone {
        start_clone_with_policy(app, url, dest);
    }
    if let Some(decision) = intent.clone_decision {
        apply_clone_decision(app, decision);
    }
    // Drain any completed preflight each frame — at most one per tab.
    drain_clone_preflight(app);
    if let Some(account_id) = intent.refresh_remote_repos {
        if let Some(account) = connected_accounts
            .iter()
            .find(|account| account.id == account_id)
            .cloned()
        {
            app.refresh_remote_repositories(&account);
        }
    }
    if let Some(account_id) = intent.load_remote_repo_owners {
        if let Some(account) = connected_accounts
            .iter()
            .find(|account| account.id == account_id)
            .cloned()
        {
            app.load_remote_repo_owners(&account);
        }
    }
    if let Some((account_id, draft)) = intent.create_remote_repo {
        if let Some(account) = connected_accounts
            .iter()
            .find(|account| account.id == account_id)
            .cloned()
        {
            app.create_remote_repository(&account, draft);
        }
    }
    if let Some(path) = intent.open_path {
        app.open_repo(&path);
    }
    if let Some(path) = intent.init_path {
        app.init_repo(&path);
    }
    if let Some(decision) = intent.init_pick_decision {
        app.apply_init_account_pick(decision);
    }
    if intent.open_settings {
        app.open_settings();
    }
}

fn render_init_account_pick(
    ui: &mut egui::Ui,
    state: &mut crate::app::WelcomeState,
    accounts: &[ProviderAccount],
    intent: &mut Intent,
    labels: &Labels,
) {
    let Some(pick) = state.pending_init_pick.as_mut() else {
        return;
    };
    ui.heading(labels.init_pick_title);
    ui.weak(labels.init_pick_hint);

    let selected_slug = pick.selected_slug.clone();
    let selected_label = selected_slug
        .as_ref()
        .and_then(|slug| accounts.iter().find(|a| a.id.slug() == *slug))
        .map(account_label)
        .unwrap_or_else(|| labels.choose_account.to_string());

    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("welcome_init_account_pick")
            .selected_text(selected_label)
            .width(280.0)
            .show_ui(ui, |ui| {
                for account in accounts {
                    let slug = account.id.slug();
                    let is_selected = selected_slug.as_deref() == Some(slug.as_str());
                    if ui
                        .selectable_label(is_selected, account_label(account))
                        .clicked()
                    {
                        pick.selected_slug = Some(slug);
                    }
                }
            });

        let can_confirm = pick.selected_slug.is_some();
        if ui
            .add_enabled(can_confirm, egui::Button::new(labels.init_pick_confirm))
            .clicked()
        {
            intent.init_pick_decision = Some(pick.selected_slug.clone());
        }
        if ui.button(labels.init_pick_skip).clicked() {
            intent.init_pick_decision = Some(None);
        }
    });
}

// ---------------- helpers ----------------

fn render_input_suggestion(
    ui: &mut egui::Ui,
    input: &str,
    default_parent: &PathBuf,
    intent: &mut Intent,
    labels: &Labels,
) {
    let input = input.trim();
    if input.is_empty() {
        return;
    }

    // Local path?
    let as_path = PathBuf::from(input);
    if as_path.is_absolute() && as_path.exists() && as_path.is_dir() {
        ui.horizontal(|ui| {
            ui.label("📁");
            if ui
                .button(format!("{} {}", labels.open_action, as_path.display()))
                .clicked()
            {
                intent.open_path = Some(as_path.clone());
            }
        });
        return;
    }

    // Git URL?
    if let Some(parsed) = git_url::parse(input) {
        let dest = default_parent.join(parsed.suggested_folder_name());
        ui.horizontal(|ui| {
            ui.label("🔗");
            ui.label(format!(
                "Clone {}/{} → {}",
                parsed.owner,
                parsed.repo,
                dest.display()
            ));
            if ui.button(labels.clone_action).clicked() {
                intent.start_clone = Some((parsed.canonical.clone(), dest));
            }
        });
    }
}

fn render_recents(
    ui: &mut egui::Ui,
    recents: &[crate::config::RecentRepo],
    input: &str,
    intent: &mut Intent,
    labels: &Labels,
) {
    ui.heading(labels.recent);
    ui.separator();
    if recents.is_empty() {
        ui.weak(labels.no_recent);
        return;
    }

    let query = recent_filter_query(input);
    egui::ScrollArea::vertical()
        .id_salt("welcome_recents_scroll")
        .max_height(280.0)
        .show(ui, |ui| {
            for r in recents {
                let name = r
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| r.path.display().to_string());

                if let Some(query) = query.as_deref() {
                    if !name.to_lowercase().contains(query)
                        && !r.path.to_string_lossy().to_lowercase().contains(query)
                    {
                        continue;
                    }
                }

                ui.horizontal(|ui| {
                    ui.label("📁");
                    if ui.link(&name).clicked() {
                        intent.open_path = Some(r.path.clone());
                    }
                    ui.weak(r.path.display().to_string());
                });
            }
        });
}

fn render_connected_repos(
    ui: &mut egui::Ui,
    state: &mut crate::app::WelcomeState,
    default_parent: &PathBuf,
    accounts: &[ProviderAccount],
    intent: &mut Intent,
    labels: &Labels,
) {
    ui.heading(labels.connected_repos);
    ui.separator();

    if accounts.is_empty() {
        ui.weak(labels.no_connected_accounts);
        ui.small(labels.connect_accounts_hint);
        return;
    }

    let selected_id = state
        .remote_repos
        .selected_account
        .clone()
        .or_else(|| accounts.first().map(|account| account.id.clone()));
    let selected_account = selected_id
        .as_ref()
        .and_then(|selected| accounts.iter().find(|account| account.id == *selected))
        .or_else(|| accounts.first());
    let create_busy = state.remote_repos.create_repo.owners_task.is_some()
        || state.remote_repos.create_repo.create_task.is_some();

    ui.horizontal(|ui| {
        let selected_label = selected_account
            .map(account_label)
            .unwrap_or_else(|| labels.choose_account.to_string());
        ui.add_enabled_ui(!create_busy, |ui| {
            egui::ComboBox::from_id_salt("welcome_repo_browser_account")
                .selected_text(selected_label)
                .width(250.0)
                .show_ui(ui, |ui| {
                    for account in accounts {
                        let selected = state
                            .remote_repos
                            .selected_account
                            .as_ref()
                            .map(|id| id == &account.id)
                            .unwrap_or(false);
                        if ui
                            .selectable_label(selected, account_label(account))
                            .clicked()
                        {
                            state.remote_repos.selected_account = Some(account.id.clone());
                            state.remote_repos.repos.clear();
                            state.remote_repos.last_error = None;
                            state.remote_repos.loaded_once = false;
                            state.remote_repos.create_repo =
                                crate::app::CreateRemoteRepoState::default();
                            intent.refresh_remote_repos = Some(account.id.clone());
                        }
                    }
                });
        });

        let refresh_clicked = ui
            .add_enabled(
                state.remote_repos.task.is_none() && selected_account.is_some() && !create_busy,
                egui::Button::new(labels.refresh_remote_repos),
            )
            .clicked();
        if refresh_clicked {
            if let Some(account) = selected_account {
                intent.refresh_remote_repos = Some(account.id.clone());
            }
        }

        let create_button = if state.remote_repos.create_repo.open {
            labels.hide_remote_repo_creator
        } else {
            labels.create_remote_repo
        };
        if ui
            .add_enabled(selected_account.is_some(), egui::Button::new(create_button))
            .clicked()
        {
            let now_open = !state.remote_repos.create_repo.open;
            state.remote_repos.create_repo.open = now_open;
            state.remote_repos.create_repo.last_error = None;
            state.remote_repos.create_repo.last_created = None;
            if now_open
                && state.remote_repos.create_repo.owners.is_empty()
                && state.remote_repos.create_repo.owners_task.is_none()
            {
                if let Some(account) = selected_account {
                    intent.load_remote_repo_owners = Some(account.id.clone());
                }
            }
        }

        if state.remote_repos.task.is_some() || create_busy {
            ui.spinner();
        }
    });

    ui.add_space(6.0);
    ui.small(labels.connected_repos_hint);
    ui.small(labels.clone_protocol_hint);
    ui.add_space(6.0);

    if state.remote_repos.create_repo.open {
        render_remote_repo_creator(
            ui,
            &mut state.remote_repos.create_repo,
            selected_account,
            default_parent,
            intent,
            labels,
        );
        ui.add_space(8.0);
    }

    if let Some(err) = &state.remote_repos.last_error {
        ui.colored_label(egui::Color32::LIGHT_RED, err);
        ui.add_space(6.0);
    }

    if state.remote_repos.task.is_some() && state.remote_repos.repos.is_empty() {
        ui.weak(labels.loading_connected_repos);
        return;
    }

    if state.remote_repos.loaded_once && state.remote_repos.repos.is_empty() {
        ui.weak(labels.no_connected_repos);
        return;
    }

    egui::ScrollArea::vertical()
        .id_salt("welcome_connected_repos_scroll")
        .max_height(280.0)
        .show(ui, |ui| {
            for repo in &state.remote_repos.repos {
                ui.group(|ui| {
                    ui.horizontal_top(|ui| {
                        ui.vertical(|ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(egui::RichText::new(repo_full_name(repo)).strong());
                                ui.small(if repo.private {
                                    labels.repo_private
                                } else {
                                    labels.repo_public
                                });
                                if let Some(branch) = &repo.default_branch {
                                    ui.small(format!("{} {branch}", labels.default_branch));
                                }
                            });
                            if let Some(description) = &repo.description {
                                if !description.trim().is_empty() {
                                    ui.small(description);
                                }
                            }
                        });

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                            if ui.button(labels.clone_action).clicked() {
                                intent.start_clone = Some((
                                    preferred_clone_url(repo).to_string(),
                                    default_parent.join(&repo.repo),
                                ));
                            }
                            if ui.small_button(labels.open_remote).clicked() {
                                ui.ctx().open_url(egui::OpenUrl::new_tab(&repo.web_url));
                            }
                        });
                    });
                });
            }
        });
}

fn render_remote_repo_creator(
    ui: &mut egui::Ui,
    create_state: &mut crate::app::CreateRemoteRepoState,
    selected_account: Option<&ProviderAccount>,
    default_parent: &PathBuf,
    intent: &mut Intent,
    labels: &Labels,
) {
    ui.group(|ui| {
        ui.label(egui::RichText::new(labels.create_remote_repo_title).strong());
        ui.small(labels.create_remote_repo_hint);
        ui.add_space(6.0);

        if let Some(created) = &create_state.last_created {
            ui.colored_label(
                egui::Color32::from_rgb(140, 210, 160),
                format!(
                    "{} {}/{}",
                    labels.created_remote_repo, created.owner, created.repo
                ),
            );
            ui.horizontal(|ui| {
                if ui.small_button(labels.open_remote).clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(&created.web_url));
                }
                if ui.small_button(labels.clone_action).clicked() {
                    intent.start_clone = Some((
                        if created.private {
                            created.clone_ssh.clone()
                        } else {
                            created.clone_https.clone()
                        },
                        default_parent.join(&created.repo),
                    ));
                }
            });
            ui.add_space(6.0);
        }

        if let Some(err) = &create_state.last_error {
            ui.colored_label(egui::Color32::LIGHT_RED, err);
            ui.add_space(6.0);
        }

        if create_state.owners_task.is_some() && create_state.owners.is_empty() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(labels.loading_remote_repo_owners);
            });
            return;
        }

        if create_state.owners.is_empty() {
            ui.weak(labels.no_remote_repo_owners);
            return;
        }

        if create_state.selected_owner.as_ref().is_none_or(|login| {
            !create_state
                .owners
                .iter()
                .any(|owner| owner.login == *login)
        }) {
            create_state.selected_owner =
                create_state.owners.first().map(|owner| owner.login.clone());
        }

        ui.horizontal(|ui| {
            ui.label(labels.remote_repo_owner);
            let owner_text = create_state
                .selected_owner
                .as_deref()
                .and_then(|login| {
                    create_state
                        .owners
                        .iter()
                        .find(|owner| owner.login == login)
                })
                .map(remote_repo_owner_label)
                .unwrap_or_else(|| labels.choose_account.to_string());
            egui::ComboBox::from_id_salt("welcome_create_remote_repo_owner")
                .selected_text(owner_text)
                .width(260.0)
                .show_ui(ui, |ui| {
                    for owner in &create_state.owners {
                        let selected = create_state.selected_owner.as_ref() == Some(&owner.login);
                        if ui
                            .selectable_label(selected, remote_repo_owner_label(owner))
                            .clicked()
                        {
                            create_state.selected_owner = Some(owner.login.clone());
                        }
                    }
                });
        });

        ui.horizontal(|ui| {
            ui.label(labels.remote_repo_name);
            ui.text_edit_singleline(&mut create_state.name);
        });
        ui.horizontal(|ui| {
            ui.label(labels.remote_repo_description);
            ui.text_edit_singleline(&mut create_state.description);
        });
        ui.checkbox(&mut create_state.private, labels.remote_repo_private);
        ui.checkbox(&mut create_state.auto_init, labels.remote_repo_auto_init);

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button(labels.hide_remote_repo_creator).clicked() {
                create_state.open = false;
            }
            let can_create = selected_account.is_some()
                && create_state.selected_owner.is_some()
                && !create_state.name.trim().is_empty();
            ui.add_enabled_ui(can_create && create_state.create_task.is_none(), |ui| {
                if ui.button(labels.create_remote_repo_submit).clicked() {
                    let Some(account) = selected_account else {
                        return;
                    };
                    let Some(owner) = create_state.selected_owner.as_ref().and_then(|login| {
                        create_state
                            .owners
                            .iter()
                            .find(|owner| owner.login == *login)
                    }) else {
                        return;
                    };
                    intent.create_remote_repo = Some((
                        account.id.clone(),
                        CreateRepositoryDraft {
                            owner: owner.login.clone(),
                            owner_kind: owner.kind,
                            name: create_state.name.trim().to_string(),
                            description: (!create_state.description.trim().is_empty())
                                .then(|| create_state.description.trim().to_string()),
                            private: create_state.private,
                            auto_init: create_state.auto_init,
                        },
                    ));
                }
            });
        });
    });
}

fn account_label(account: &ProviderAccount) -> String {
    format!(
        "{} ({})",
        account.display_name,
        provider_host_label(&account.id.kind)
    )
}

fn remote_repo_owner_label(owner: &RemoteRepoOwner) -> String {
    let kind = match owner.kind {
        RemoteRepoOwnerKind::User => "account",
        RemoteRepoOwnerKind::Organization => "org",
    };
    if owner.display_name == owner.login {
        format!("{} ({kind})", owner.login)
    } else {
        format!("{} ({}, {kind})", owner.display_name, owner.login)
    }
}

fn provider_host_label(kind: &providers::ProviderKind) -> String {
    match kind {
        providers::ProviderKind::GitHub => "GitHub".to_string(),
        providers::ProviderKind::GitLab => "GitLab".to_string(),
        providers::ProviderKind::Bitbucket => "Bitbucket".to_string(),
        providers::ProviderKind::AzureDevOps => "Azure DevOps".to_string(),
        providers::ProviderKind::Codeberg => "Codeberg".to_string(),
        providers::ProviderKind::Gitea { instance } => instance.clone(),
        providers::ProviderKind::Generic { host } => host.clone(),
    }
}

fn repo_full_name(repo: &RemoteRepoSummary) -> String {
    format!("{}/{}", repo.owner, repo.repo)
}

fn preferred_clone_url(repo: &RemoteRepoSummary) -> &str {
    if repo.private {
        &repo.clone_ssh
    } else {
        &repo.clone_https
    }
}

struct Labels {
    tagline: &'static str,
    input_hint: &'static str,
    open_local_folder: &'static str,
    clone_from_url: &'static str,
    /// "Init" button — run `git init` in a chosen folder, then open it.
    init_new_folder: &'static str,
    init_new_folder_hint: &'static str,
    settings: &'static str,
    recent: &'static str,
    no_recent: &'static str,
    open_action: &'static str,
    clone_action: &'static str,
    connected_repos: &'static str,
    no_connected_accounts: &'static str,
    connect_accounts_hint: &'static str,
    connected_repos_hint: &'static str,
    clone_protocol_hint: &'static str,
    choose_account: &'static str,
    refresh_remote_repos: &'static str,
    create_remote_repo: &'static str,
    hide_remote_repo_creator: &'static str,
    create_remote_repo_title: &'static str,
    create_remote_repo_hint: &'static str,
    loading_remote_repo_owners: &'static str,
    no_remote_repo_owners: &'static str,
    remote_repo_owner: &'static str,
    remote_repo_name: &'static str,
    remote_repo_description: &'static str,
    remote_repo_private: &'static str,
    remote_repo_auto_init: &'static str,
    create_remote_repo_submit: &'static str,
    created_remote_repo: &'static str,
    loading_connected_repos: &'static str,
    no_connected_repos: &'static str,
    repo_private: &'static str,
    repo_public: &'static str,
    default_branch: &'static str,
    open_remote: &'static str,
    /// Shown while a pre-clone size probe is in flight.
    checking_size: &'static str,
    /// Heading of the large-repo prompt.
    clone_large_title: &'static str,
    /// Button label: do a shallow clone.
    clone_shallow_btn: &'static str,
    /// Button label: do a full clone anyway.
    clone_full_btn: &'static str,
    /// Button label: dismiss the prompt without cloning.
    clone_cancel_btn: &'static str,
    /// Suffix shown after the "Shallow clone (depth N)" button tooltip.
    clone_shallow_hint: &'static str,
    /// Heading of the post-`git init` account picker.
    init_pick_title: &'static str,
    /// Subtext under the picker heading explaining why we're asking.
    init_pick_hint: &'static str,
    init_pick_confirm: &'static str,
    init_pick_skip: &'static str,
}

fn labels(language: UiLanguage) -> Labels {
    match language {
        UiLanguage::Korean => Labels {
            tagline: "가벼운 Git 클라이언트",
            input_hint: "git URL을 붙여넣거나, 경로를 입력하거나, 최근 저장소를 검색하세요…",
            open_local_folder: "📁 로컬 폴더 열기…",
            init_new_folder: "🆕 폴더에서 저장소 초기화…",
            init_new_folder_hint: "선택한 폴더에서 `git init`을 실행하고 새 저장소로 엽니다.",
            clone_from_url: "🔗 URL로 클론…",
            settings: "⚙ 설정",
            recent: "최근 저장소",
            no_recent: "최근에 연 저장소가 없습니다.",
            open_action: "열기",
            clone_action: "클론",
            connected_repos: "연결된 원격 저장소",
            no_connected_accounts: "연결된 계정이 없습니다.",
            connect_accounts_hint: "설정 → 연동에서 Git 호스트 계정을 연결하면 여기에서 바로 클론할 수 있습니다.",
            connected_repos_hint: "내 계정과 내가 속한 조직에서 접근 가능한 저장소를 보여줍니다.",
            clone_protocol_hint: "클론은 공개 저장소는 HTTPS, 비공개 저장소는 SSH URL을 기본으로 사용합니다.",
            choose_account: "계정 선택",
            refresh_remote_repos: "새로고침",
            create_remote_repo: "원격 저장소 만들기",
            hide_remote_repo_creator: "닫기",
            create_remote_repo_title: "연결된 계정/조직에 새 저장소 만들기",
            create_remote_repo_hint:
                "선택한 계정의 개인 계정이나 조직 아래에 새 원격 저장소를 만듭니다.",
            loading_remote_repo_owners: "계정과 조직 목록을 불러오는 중…",
            no_remote_repo_owners: "이 계정으로 생성 가능한 대상이 없습니다.",
            remote_repo_owner: "소유자:",
            remote_repo_name: "저장소 이름:",
            remote_repo_description: "설명:",
            remote_repo_private: "비공개 저장소",
            remote_repo_auto_init: "README로 초기화",
            create_remote_repo_submit: "저장소 생성",
            created_remote_repo: "생성됨:",
            loading_connected_repos: "원격 저장소 목록을 불러오는 중…",
            no_connected_repos: "이 계정으로 볼 수 있는 저장소가 없습니다.",
            repo_private: "private",
            repo_public: "public",
            default_branch: "기본 브랜치:",
            open_remote: "원격 열기",
            checking_size: "저장소 크기를 확인하는 중…",
            clone_large_title: "큰 저장소 감지",
            clone_shallow_btn: "Shallow 클론",
            clone_full_btn: "전체 클론",
            clone_cancel_btn: "취소",
            clone_shallow_hint: "최근 커밋만 받습니다. 나중에 전체 히스토리를 받아올 수 있습니다.",
            init_pick_title: "업스트림 계정 선택",
            init_pick_hint: "새 저장소의 기본 업스트림으로 사용할 연결된 계정을 선택하세요. 나중에 저장소 설정에서 바꿀 수 있습니다.",
            init_pick_confirm: "이 계정 사용",
            init_pick_skip: "건너뛰기",
        },
        UiLanguage::Japanese => Labels {
            tagline: "軽量な Git クライアント",
            input_hint:
                "git URL を貼り付けるか、パスを入力するか、最近のリポジトリを検索してください…",
            open_local_folder: "📁 ローカルフォルダを開く…",
            init_new_folder: "🆕 フォルダでリポジトリを初期化…",
            init_new_folder_hint: "選択したフォルダで `git init` を実行し、新しいリポジトリとして開きます。",
            clone_from_url: "🔗 URL からクローン…",
            settings: "⚙ 設定",
            recent: "最近のリポジトリ",
            no_recent: "最近開いたリポジトリはありません。",
            open_action: "開く",
            clone_action: "クローン",
            connected_repos: "接続済みリモートリポジトリ",
            no_connected_accounts: "接続済みアカウントがありません。",
            connect_accounts_hint: "設定 → 連携 で Git ホストのアカウントを接続すると、ここから直接クローンできます。",
            connected_repos_hint: "自分のアカウントと所属組織でアクセス可能なリポジトリを表示します。",
            clone_protocol_hint: "クローンは公開リポジトリでは HTTPS、非公開リポジトリでは SSH URL を既定で使います。",
            choose_account: "アカウントを選択",
            refresh_remote_repos: "再読み込み",
            create_remote_repo: "Create remote repo",
            hide_remote_repo_creator: "Close",
            create_remote_repo_title: "Create a new remote repository",
            create_remote_repo_hint:
                "Create a repository under this account or one of its organizations.",
            loading_remote_repo_owners: "Loading accounts and organizations…",
            no_remote_repo_owners: "No repository owners are available for this account.",
            remote_repo_owner: "Owner:",
            remote_repo_name: "Repository name:",
            remote_repo_description: "Description:",
            remote_repo_private: "Private repository",
            remote_repo_auto_init: "Initialize with README",
            create_remote_repo_submit: "Create repository",
            created_remote_repo: "Created:",
            loading_connected_repos: "リモートリポジトリを読み込み中…",
            no_connected_repos: "このアカウントで閲覧できるリポジトリはありません。",
            repo_private: "private",
            repo_public: "public",
            default_branch: "既定ブランチ:",
            open_remote: "リモートを開く",
            checking_size: "リポジトリサイズを確認中…",
            clone_large_title: "大きなリポジトリを検出",
            clone_shallow_btn: "Shallow クローン",
            clone_full_btn: "フルクローン",
            clone_cancel_btn: "キャンセル",
            clone_shallow_hint: "直近のコミットのみ取得します。あとで完全な履歴を取得できます。",
            init_pick_title: "アップストリームアカウントを選択",
            init_pick_hint: "この新しいリポジトリの既定アップストリームとして使用する接続済みアカウントを選択してください。後でリポジトリ設定から変更できます。",
            init_pick_confirm: "このアカウントを使用",
            init_pick_skip: "スキップ",
        },
        UiLanguage::Chinese => Labels {
            tagline: "轻量级 Git 客户端",
            input_hint: "粘贴 git URL、输入路径，或搜索最近仓库…",
            open_local_folder: "📁 打开本地文件夹…",
            init_new_folder: "🆕 在文件夹中初始化仓库…",
            init_new_folder_hint: "在所选文件夹中运行 `git init` 并作为新仓库打开。",
            clone_from_url: "🔗 从 URL 克隆…",
            settings: "⚙ 设置",
            recent: "最近仓库",
            no_recent: "还没有最近打开的仓库。",
            open_action: "打开",
            clone_action: "克隆",
            connected_repos: "已连接的远程仓库",
            no_connected_accounts: "还没有已连接账号。",
            connect_accounts_hint: "在 设置 → 集成 中连接 Git 主机账号后，就可以在这里直接克隆。",
            connected_repos_hint: "显示你的账号以及所属组织中可访问的仓库。",
            clone_protocol_hint: "默认对公开仓库使用 HTTPS，对私有仓库使用 SSH URL。",
            choose_account: "选择账号",
            refresh_remote_repos: "刷新",
            create_remote_repo: "Create remote repo",
            hide_remote_repo_creator: "Close",
            create_remote_repo_title: "Create a new remote repository",
            create_remote_repo_hint:
                "Create a repository under this account or one of its organizations.",
            loading_remote_repo_owners: "Loading accounts and organizations…",
            no_remote_repo_owners: "No repository owners are available for this account.",
            remote_repo_owner: "Owner:",
            remote_repo_name: "Repository name:",
            remote_repo_description: "Description:",
            remote_repo_private: "Private repository",
            remote_repo_auto_init: "Initialize with README",
            create_remote_repo_submit: "Create repository",
            created_remote_repo: "Created:",
            loading_connected_repos: "正在加载远程仓库…",
            no_connected_repos: "该账号下没有可见仓库。",
            repo_private: "private",
            repo_public: "public",
            default_branch: "默认分支:",
            open_remote: "打开远程页面",
            checking_size: "正在获取仓库大小…",
            clone_large_title: "检测到大型仓库",
            clone_shallow_btn: "浅克隆",
            clone_full_btn: "完整克隆",
            clone_cancel_btn: "取消",
            clone_shallow_hint: "仅获取最近的提交。之后可再获取完整历史。",
            init_pick_title: "选择上游账号",
            init_pick_hint: "选择一个已连接账号作为此新仓库的默认上游。之后可在仓库设置中修改。",
            init_pick_confirm: "使用此账号",
            init_pick_skip: "跳过",
        },
        UiLanguage::French => Labels {
            tagline: "client Git léger",
            input_hint: "collez une URL git, saisissez un chemin ou recherchez dans les récents…",
            open_local_folder: "📁 Ouvrir un dossier local…",
            init_new_folder: "🆕 Initialiser un dépôt dans un dossier…",
            init_new_folder_hint: "Exécute `git init` dans le dossier choisi puis l'ouvre comme dépôt.",
            clone_from_url: "🔗 Cloner depuis une URL…",
            settings: "⚙ Paramètres",
            recent: "Récents",
            no_recent: "Aucun dépôt récent.",
            open_action: "Ouvrir",
            clone_action: "Cloner",
            connected_repos: "Dépôts distants connectés",
            no_connected_accounts: "Aucun compte connecté.",
            connect_accounts_hint: "Connectez un compte Git dans Paramètres → Intégrations pour cloner directement depuis ici.",
            connected_repos_hint: "Affiche les dépôts accessibles depuis votre compte et vos organisations.",
            clone_protocol_hint: "Le clonage utilise HTTPS pour les dépôts publics et SSH par défaut pour les dépôts privés.",
            choose_account: "Choisir un compte",
            refresh_remote_repos: "Actualiser",
            create_remote_repo: "Create remote repo",
            hide_remote_repo_creator: "Close",
            create_remote_repo_title: "Create a new remote repository",
            create_remote_repo_hint:
                "Create a repository under this account or one of its organizations.",
            loading_remote_repo_owners: "Loading accounts and organizations…",
            no_remote_repo_owners: "No repository owners are available for this account.",
            remote_repo_owner: "Owner:",
            remote_repo_name: "Repository name:",
            remote_repo_description: "Description:",
            remote_repo_private: "Private repository",
            remote_repo_auto_init: "Initialize with README",
            create_remote_repo_submit: "Create repository",
            created_remote_repo: "Created:",
            loading_connected_repos: "Chargement des dépôts distants…",
            no_connected_repos: "Aucun dépôt visible pour ce compte.",
            repo_private: "private",
            repo_public: "public",
            default_branch: "Branche par défaut :",
            open_remote: "Ouvrir le dépôt",
            checking_size: "Vérification de la taille du dépôt…",
            clone_large_title: "Dépôt volumineux détecté",
            clone_shallow_btn: "Clone superficiel",
            clone_full_btn: "Clone complet",
            clone_cancel_btn: "Annuler",
            clone_shallow_hint: "Ne récupère que les derniers commits. Vous pourrez récupérer tout l'historique plus tard.",
            init_pick_title: "Choisir un compte amont",
            init_pick_hint: "Choisissez un compte connecté à utiliser comme amont par défaut pour ce nouveau dépôt. Vous pourrez le changer plus tard dans les paramètres du dépôt.",
            init_pick_confirm: "Utiliser ce compte",
            init_pick_skip: "Ignorer",
        },
        UiLanguage::Spanish => Labels {
            tagline: "cliente Git ligero",
            input_hint: "pega una URL git, escribe una ruta o busca en recientes…",
            open_local_folder: "📁 Abrir carpeta local…",
            init_new_folder: "🆕 Inicializar repo en una carpeta…",
            init_new_folder_hint: "Ejecuta `git init` en la carpeta elegida y la abre como repositorio.",
            clone_from_url: "🔗 Clonar desde URL…",
            settings: "⚙ Ajustes",
            recent: "Recientes",
            no_recent: "Todavía no hay repositorios recientes.",
            open_action: "Abrir",
            clone_action: "Clonar",
            connected_repos: "Repos remotos conectados",
            no_connected_accounts: "No hay cuentas conectadas.",
            connect_accounts_hint: "Conecta una cuenta Git en Ajustes → Integraciones para clonar directamente desde aquí.",
            connected_repos_hint: "Muestra los repos accesibles desde tu cuenta y tus organizaciones.",
            clone_protocol_hint: "El clon usa HTTPS para repos públicos y SSH por defecto para repos privados.",
            choose_account: "Elegir cuenta",
            refresh_remote_repos: "Actualizar",
            create_remote_repo: "Create remote repo",
            hide_remote_repo_creator: "Close",
            create_remote_repo_title: "Create a new remote repository",
            create_remote_repo_hint:
                "Create a repository under this account or one of its organizations.",
            loading_remote_repo_owners: "Loading accounts and organizations…",
            no_remote_repo_owners: "No repository owners are available for this account.",
            remote_repo_owner: "Owner:",
            remote_repo_name: "Repository name:",
            remote_repo_description: "Description:",
            remote_repo_private: "Private repository",
            remote_repo_auto_init: "Initialize with README",
            create_remote_repo_submit: "Create repository",
            created_remote_repo: "Created:",
            loading_connected_repos: "Cargando repos remotos…",
            no_connected_repos: "No hay repos visibles para esta cuenta.",
            repo_private: "private",
            repo_public: "public",
            default_branch: "Rama por defecto:",
            open_remote: "Abrir remoto",
            checking_size: "Comprobando tamaño del repositorio…",
            clone_large_title: "Repositorio grande detectado",
            clone_shallow_btn: "Clon superficial",
            clone_full_btn: "Clon completo",
            clone_cancel_btn: "Cancelar",
            clone_shallow_hint: "Solo descarga los commits recientes. Puedes obtener el historial completo después.",
            init_pick_title: "Elegir cuenta upstream",
            init_pick_hint: "Elige una cuenta conectada como upstream predeterminado para este nuevo repositorio. Podrás cambiarlo después en los ajustes del repositorio.",
            init_pick_confirm: "Usar esta cuenta",
            init_pick_skip: "Omitir",
        },
        _ => Labels {
            tagline: "lightweight git client",
            input_hint: "paste git URL, type path, or search recents…",
            open_local_folder: "📁 Open local folder…",
            clone_from_url: "🔗 Clone from URL…",
            init_new_folder: "🆕 Initialize in folder…",
            init_new_folder_hint:
                "Run `git init` in the chosen folder and open it as a new repository.",
            settings: "⚙ Settings",
            recent: "Recent",
            no_recent: "No recent repositories yet.",
            open_action: "Open",
            clone_action: "Clone",
            connected_repos: "Connected Remote Repositories",
            no_connected_accounts: "No connected accounts yet.",
            connect_accounts_hint: "Connect a Git host account in Settings → Integrations to clone directly from here.",
            connected_repos_hint: "Shows repositories you can access from your account and organizations.",
            clone_protocol_hint: "Clone uses HTTPS for public repositories and SSH for private repositories by default.",
            choose_account: "Choose account",
            refresh_remote_repos: "Refresh",
            create_remote_repo: "Create remote repo",
            hide_remote_repo_creator: "Close",
            create_remote_repo_title: "Create a new remote repository",
            create_remote_repo_hint:
                "Create a repository under this account or one of its organizations.",
            loading_remote_repo_owners: "Loading accounts and organizations…",
            no_remote_repo_owners: "No repository owners are available for this account.",
            remote_repo_owner: "Owner:",
            remote_repo_name: "Repository name:",
            remote_repo_description: "Description:",
            remote_repo_private: "Private repository",
            remote_repo_auto_init: "Initialize with README",
            create_remote_repo_submit: "Create repository",
            created_remote_repo: "Created:",
            loading_connected_repos: "Loading remote repositories…",
            no_connected_repos: "No repositories are visible for this account.",
            repo_private: "private",
            repo_public: "public",
            default_branch: "Default branch:",
            open_remote: "Open remote",
            checking_size: "Checking repository size…",
            clone_large_title: "Large repository detected",
            clone_shallow_btn: "Shallow clone",
            clone_full_btn: "Full clone",
            clone_cancel_btn: "Cancel",
            clone_shallow_hint: "Download recent commits only. You can fetch the full history later.",
            init_pick_title: "Choose upstream account",
            init_pick_hint: "Pick a connected account to use as this new repository's default upstream. You can change this later in repo settings.",
            init_pick_confirm: "Use this account",
            init_pick_skip: "Skip",
        },
    }
}

/// Render the "Large repository mode (advanced)" collapsible beneath
/// the unified input on the welcome page.
///
/// Design notes
/// ------------
/// * Collapsed by default. Anyone who doesn't need this feature shouldn't
///   see it — the default full-clone path is unchanged.
/// * The panel is purely UI state. We don't dispatch a clone from here —
///   dispatch happens through the normal "Clone" button. This panel's
///   job is to seed `state.large_repo_mode`, which
///   `resolve_large_repo_opts` reads at dispatch time.
/// * Localisation scope: deliberately English-only for this first cut.
///   Adding this surface to every `Labels` locale would dwarf the rest
///   of the change; the rest of the wizard already mixes English in a
///   few locales (remote-repo creator, for instance). We can add
///   translations in a follow-up.
fn render_large_repo_mode_panel(
    ui: &mut egui::Ui,
    state: &mut LargeRepoModeState,
    defaults: &crate::config::CloneDefaults,
    _labels: &Labels,
) {
    // Seed from persisted defaults once, then leave alone. Seeding on
    // every frame would stomp user edits; seeding only when the user
    // first expands keeps the starting point sensible.
    if !state.seeded {
        state.shallow_depth = defaults.shallow_depth;
        state.mode = match defaults.filter_policy {
            CloneFilterPolicy::None => LargeRepoMode::Full,
            CloneFilterPolicy::BlobNone => LargeRepoMode::PartialBlobNone,
            CloneFilterPolicy::BlobLimit => LargeRepoMode::PartialBlobLimit,
            CloneFilterPolicy::TreeZero => LargeRepoMode::PartialTreeZero,
        };
        // Default 1 MiB split as 1 MB for display.
        let bytes = defaults.blob_limit_bytes.max(1);
        if bytes % (1024 * 1024) == 0 {
            state.blob_limit_unit = BlobLimitUnit::Mb;
            state.blob_limit_value = (bytes / (1024 * 1024)).min(u32::MAX as u64) as u32;
        } else {
            state.blob_limit_unit = BlobLimitUnit::Kb;
            state.blob_limit_value = (bytes / 1024).max(1).min(u32::MAX as u64) as u32;
        }
        if state.blob_limit_value == 0 {
            state.blob_limit_value = 1;
        }
        state.seeded = true;
    }

    egui::CollapsingHeader::new("Large repository mode (advanced)")
        .default_open(state.expanded)
        .show(ui, |ui| {
            state.expanded = true;
            ui.label(
                egui::RichText::new(
                    "Use for monorepos, game engines, and large-binary repositories. \
                     These options only apply to the next clone you start.",
                )
                .weak()
                .size(11.0),
            );
            ui.add_space(4.0);

            ui.radio_value(&mut state.mode, LargeRepoMode::Full, "Full clone");
            ui.radio_value(&mut state.mode, LargeRepoMode::Shallow, "Shallow");
            ui.radio_value(
                &mut state.mode,
                LargeRepoMode::PartialBlobNone,
                "Partial (blob:none) — fetch blobs on demand",
            );
            ui.radio_value(
                &mut state.mode,
                LargeRepoMode::PartialBlobLimit,
                "Partial (blob:limit) — skip blobs above a size threshold",
            );
            ui.radio_value(
                &mut state.mode,
                LargeRepoMode::PartialTreeZero,
                "Partial + sparse (tree:0) — fetch only the directories you pick",
            );

            // Depth input — visible for Shallow always, and for the
            // partial modes when the user opts in to combine the two.
            ui.add_space(6.0);
            match state.mode {
                LargeRepoMode::Shallow => {
                    ui.horizontal(|ui| {
                        ui.label("Depth:");
                        ui.add(egui::DragValue::new(&mut state.shallow_depth).clamp_range(1..=u32::MAX));
                    });
                }
                LargeRepoMode::PartialBlobNone
                | LargeRepoMode::PartialBlobLimit
                | LargeRepoMode::PartialTreeZero => {
                    ui.checkbox(
                        &mut state.combine_shallow,
                        "Also limit history depth (--depth=N)",
                    );
                    if state.combine_shallow {
                        ui.horizontal(|ui| {
                            ui.label("Depth:");
                            ui.add(
                                egui::DragValue::new(&mut state.shallow_depth)
                                    .clamp_range(1..=u32::MAX),
                            );
                        });
                    }
                }
                LargeRepoMode::Full => {}
            }

            if state.mode == LargeRepoMode::PartialBlobLimit {
                ui.horizontal(|ui| {
                    ui.label("Size threshold:");
                    ui.add(
                        egui::DragValue::new(&mut state.blob_limit_value)
                            .clamp_range(1..=u32::MAX),
                    );
                    egui::ComboBox::from_id_source("blob-limit-unit")
                        .selected_text(state.blob_limit_unit.label())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut state.blob_limit_unit,
                                BlobLimitUnit::Kb,
                                "KB",
                            );
                            ui.selectable_value(
                                &mut state.blob_limit_unit,
                                BlobLimitUnit::Mb,
                                "MB",
                            );
                        });
                    ui.weak("blobs smaller than this are fetched eagerly");
                });
            }

            if state.mode == LargeRepoMode::PartialTreeZero {
                ui.label("Sparse-checkout directories (one per line):");
                ui.add(
                    egui::TextEdit::multiline(&mut state.sparse_dirs_text)
                        .desired_rows(4)
                        .desired_width(560.0)
                        .hint_text("src/\nREADME.md"),
                );
                if let Some(msg) = state.validation_error.as_ref() {
                    ui.colored_label(egui::Color32::LIGHT_RED, msg);
                }
                ui.weak(
                    "Cone mode — list directories (not globs). \
                     At least one entry is required for tree:0 clones.",
                );
            }
        });
}

/// Parse the sparse-dirs textarea, stripping blank lines and trimming
/// whitespace. Returns a list suitable to hand to `git sparse-checkout
/// set` directly.
fn parse_sparse_dirs(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

/// Translate the UI state into `CloneOpts`. Validation lives here so
/// both the Clone button and the large-repo prompt's "Partial" button
/// see identical rules.
///
/// Returns `Err(msg)` when the user selected a combination we can't
/// honour (currently: `TreeZero` with no sparse directories). The
/// caller is responsible for routing that error into
/// `state.large_repo_mode.validation_error` and aborting the clone.
fn resolve_large_repo_opts(state: &LargeRepoModeState) -> Result<CloneOpts, String> {
    let depth = match state.mode {
        LargeRepoMode::Full => None,
        LargeRepoMode::Shallow => Some(state.shallow_depth.max(1)),
        LargeRepoMode::PartialBlobNone
        | LargeRepoMode::PartialBlobLimit
        | LargeRepoMode::PartialTreeZero => {
            if state.combine_shallow {
                Some(state.shallow_depth.max(1))
            } else {
                None
            }
        }
    };

    let filter = match state.mode {
        LargeRepoMode::Full | LargeRepoMode::Shallow => CloneFilter::None,
        LargeRepoMode::PartialBlobNone => CloneFilter::BlobNone,
        LargeRepoMode::PartialBlobLimit => CloneFilter::BlobLimit {
            bytes: state
                .blob_limit_unit
                .to_bytes(state.blob_limit_value.max(1)),
        },
        LargeRepoMode::PartialTreeZero => CloneFilter::TreeZero,
    };

    let sparse_dirs = if state.mode == LargeRepoMode::PartialTreeZero {
        parse_sparse_dirs(&state.sparse_dirs_text)
    } else {
        Vec::new()
    };

    if matches!(filter, CloneFilter::TreeZero) && sparse_dirs.is_empty() {
        return Err(
            "Add at least one sparse-checkout directory before cloning (one per line)."
                .to_string(),
        );
    }

    Ok(CloneOpts {
        depth,
        filter,
        sparse_dirs,
    })
}

/// Render the "this repo is big, how do you want to clone it?" modal.
/// Kept inline on the welcome page (no `egui::Window`) so it reads like a
/// banner rather than a popup: the user is already staring at the clone
/// target, no need to context-switch.
fn render_clone_size_prompt(
    ui: &mut egui::Ui,
    prompt: &CloneSizePrompt,
    labels: &Labels,
    intent: &mut Intent,
) {
    ui.heading(labels.clone_large_title);
    ui.weak(format!(
        "{} — {}",
        prompt.url,
        format_size_mb(prompt.size_bytes),
    ));
    ui.weak(labels.clone_shallow_hint);

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.button(labels.clone_shallow_btn).clicked() {
            intent.clone_decision = Some(CloneDecision::Shallow);
        }
        // "Partial" is the recommended response for a large repo where
        // the bulk of the bytes are blobs (assets, generated
        // artifacts). We label it as "Partial" — the user learns the
        // technical name via the tooltip / the post-clone banner.
        if ui
            .button("Partial (blob:none)")
            .on_hover_text(
                "Fetch commits and trees now, blobs on demand. \
                 Usually the best trade-off for binary-heavy repos.",
            )
            .clicked()
        {
            intent.clone_decision = Some(CloneDecision::PartialBlobNone);
        }
        if ui.button(labels.clone_full_btn).clicked() {
            intent.clone_decision = Some(CloneDecision::Full);
        }
        if ui.button(labels.clone_cancel_btn).clicked() {
            intent.clone_decision = Some(CloneDecision::Cancel);
        }
        ui.weak(format!("(depth {})", prompt.shallow_depth));
    });
}

fn format_size_mb(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else {
        format!("{} MB", bytes / MB)
    }
}

/// Dispatch a Clone-button click according to the user's configured
/// `CloneSizePolicy`. The welcome flow never mutates `state.clone` or
/// `state.clone_preflight` directly — they all pass through here and
/// through `apply_clone_decision`.
///
/// Advanced-panel semantics
/// ------------------------
/// When the user has configured anything in the Large-repository-mode
/// panel (non-`Full` radio selection, or sparse dirs typed in), we
/// bypass the `CloneSizePolicy` preflight entirely — the user has
/// already stated an intention, so we honour it without asking "do you
/// want to go shallow?" on top of it. The preflight probe still runs
/// when the panel is left at defaults.
fn start_clone_with_policy(app: &mut MergeFoxApp, url: String, dest: PathBuf) {
    let policy = app.config.clone_defaults.size_policy;
    let shallow_depth = app.config.clone_defaults.shallow_depth;
    let threshold_mb = app.config.clone_defaults.prompt_threshold_mb;
    let accounts = app.config.provider_accounts.clone();

    // Resolve advanced-panel options before we borrow `state` mutably
    // so the validation error can be surfaced back on the same state.
    let advanced_opts = app
        .active_welcome_state()
        .map(|s| (s.large_repo_mode.mode, resolve_large_repo_opts(&s.large_repo_mode)));

    let Some(state) = app.active_welcome_state_mut() else {
        return;
    };
    if state.clone.is_some() || state.clone_preflight.is_some() {
        return; // already doing something
    }

    // Clear any stale validation from a previous submit.
    state.large_repo_mode.validation_error = None;

    let advanced_in_effect = match &advanced_opts {
        Some((mode, _)) => *mode != LargeRepoMode::Full,
        None => false,
    };

    // Hand-configured advanced mode short-circuits the preflight — the
    // user has already picked how they want to clone.
    if advanced_in_effect {
        let Some((_, resolved)) = advanced_opts else {
            return;
        };
        match resolved {
            Ok(opts) => {
                state.clone = Some(clone::spawn_with_opts(url, dest, opts, accounts));
            }
            Err(msg) => {
                state.large_repo_mode.validation_error = Some(msg);
            }
        }
        return;
    }

    match policy {
        CloneSizePolicy::AlwaysFull => {
            state.clone = Some(clone::spawn(url, dest, None, accounts));
        }
        CloneSizePolicy::AlwaysShallow => {
            state.clone = Some(clone::spawn(url, dest, Some(shallow_depth), accounts));
        }
        CloneSizePolicy::Prompt => {
            // Only meaningful to preflight for hosts where we know how
            // to query size. For everything else we skip the probe (no
            // latency cost) and do a full clone. The user gets the
            // prompt only when we can back it with a real number.
            let parsed = git_url::parse(&url);
            let host = parsed.as_ref().map(|p| p.host.clone());
            let owner = parsed.as_ref().map(|p| p.owner.clone());
            let repo = parsed.as_ref().map(|p| p.repo.clone());
            let can_probe = matches!(host.as_deref(), Some("github.com") | Some("gitlab.com"));
            if can_probe {
                // Double unwraps are safe because can_probe implies parsed.
                state.clone_preflight = Some(clone::spawn_preflight(
                    url,
                    dest,
                    host.unwrap(),
                    owner.unwrap(),
                    repo.unwrap(),
                ));
                // Store threshold on state so drain_clone_preflight can
                // read it without re-fetching config (avoids a borrow
                // dance). Threshold lives on config though — we read
                // again in drain via app.config.
                let _ = threshold_mb;
                let _ = shallow_depth;
            } else {
                state.clone = Some(clone::spawn(url, dest, None, accounts));
            }
        }
    }
}

/// Apply the user's choice in the large-repo prompt.
fn apply_clone_decision(app: &mut MergeFoxApp, decision: CloneDecision) {
    let shallow_depth = app.config.clone_defaults.shallow_depth;
    let accounts = app.config.provider_accounts.clone();
    let Some(state) = app.active_welcome_state_mut() else {
        return;
    };
    let Some(prompt) = state.clone_size_prompt.take() else {
        return;
    };
    match decision {
        CloneDecision::Full => {
            state.clone = Some(clone::spawn(prompt.url, prompt.dest, None, accounts));
        }
        CloneDecision::Shallow => {
            state.clone = Some(clone::spawn(
                prompt.url,
                prompt.dest,
                Some(shallow_depth),
                accounts,
            ));
        }
        CloneDecision::PartialBlobNone => {
            // Recommended one-click answer for big repos: keep the full
            // commit/tree graph (so `git log`, `git blame`, branch
            // navigation still work offline) but lazy-fetch blobs. This
            // is usually 10–20× less data than a full clone on asset-
            // heavy projects, and the user can still promote to full
            // later with `git fetch --refetch --filter=…`.
            let opts = CloneOpts {
                filter: CloneFilter::BlobNone,
                ..CloneOpts::default()
            };
            state.clone = Some(clone::spawn_with_opts(
                prompt.url,
                prompt.dest,
                opts,
                accounts,
            ));
        }
        CloneDecision::Cancel => {
            // Just dropping the prompt above is enough.
        }
    }
}

/// Check the active welcome state's preflight, if any, and promote its
/// result into either an immediate clone (below threshold / unknown
/// policy) or a `CloneSizePrompt` (above threshold).
fn drain_clone_preflight(app: &mut MergeFoxApp) {
    let threshold_bytes = (app.config.clone_defaults.prompt_threshold_mb as u64) * 1024 * 1024;
    let shallow_depth = app.config.clone_defaults.shallow_depth;
    let accounts = app.config.provider_accounts.clone();
    let Some(state) = app.active_welcome_state_mut() else {
        return;
    };
    let Some(handle) = state.clone_preflight.as_ref() else {
        return;
    };
    let Some(outcome) = handle.poll() else {
        return;
    };
    // Consume the handle; we're done polling it.
    let preflight = state.clone_preflight.take().expect("present above");
    match outcome {
        clone::PreflightOutcome::KnownSize { bytes } if bytes >= threshold_bytes => {
            state.clone_size_prompt = Some(CloneSizePrompt {
                url: preflight.url,
                dest: preflight.dest,
                size_bytes: bytes,
                shallow_depth,
            });
        }
        // Below threshold or unknown — proceed with a normal full clone.
        // "Unknown" + "Prompt policy" is a deliberate no-prompt path: if
        // we can't back the warning with a real number we don't ask.
        _ => {
            state.clone = Some(clone::spawn(preflight.url, preflight.dest, None, accounts));
        }
    }
}

fn render_clone_progress(ui: &mut egui::Ui, handle: &crate::clone::CloneHandle) {
    let p = handle.snapshot();
    let frac = if p.total_objects > 0 {
        p.received_objects as f32 / p.total_objects as f32
    } else {
        0.0
    };
    let stage_label = match p.stage {
        Stage::Connecting => "connecting",
        Stage::Receiving => "receiving",
        Stage::Resolving => "resolving",
        Stage::Checkout => "checkout",
    };
    ui.label(format!(
        "⏳ cloning {} ({stage_label}, {}/{}, {:.1} MB)",
        handle.url,
        p.received_objects,
        p.total_objects,
        p.received_bytes as f64 / 1_048_576.0
    ));
    ui.add(egui::ProgressBar::new(frac).show_percentage());
}

fn recent_filter_query(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() || looks_like_clone_source(trimmed) {
        None
    } else {
        Some(trimmed.to_lowercase())
    }
}

fn looks_like_clone_source(input: &str) -> bool {
    git_url::parse(input).is_some()
        || input.contains("://")
        || input.starts_with("git@")
        || input.starts_with("ssh://")
}
