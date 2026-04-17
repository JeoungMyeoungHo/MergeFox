use crate::app::{MergeFoxApp, View};
use crate::config::UiLanguage;
use crate::providers::{
    AccountId, CreateRepositoryDraft, ProviderAccount, RemoteRepoOwner, RemoteRepoOwnerKind,
};

#[derive(Default)]
struct Intent {
    close: bool,
    open_settings: bool,
    load_owners: Option<AccountId>,
    create: Option<(AccountId, CreateRepositoryDraft)>,
}

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    if app.publish_remote_modal.is_none() {
        return;
    }

    let accounts = app.repo_browser_accounts();
    let labels = labels(app.config.ui_language.resolved());
    let mut intent = Intent::default();

    let active_repo_job = if let Some(modal) = app.publish_remote_modal.as_ref() {
        match &app.view {
            View::Workspace(tabs) => tabs
                .tabs
                .iter()
                .find(|ws| ws.repo.path() == modal.repo_path)
                .and_then(|ws| ws.active_job.as_ref())
                .is_some(),
            _ => false,
        }
    } else {
        false
    };

    egui::Window::new(labels.title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .default_width(440.0)
        .show(ctx, |ui| {
            let Some(modal) = app.publish_remote_modal.as_mut() else {
                return;
            };

            ui.label(format!(
                "{} `{}` {}",
                labels.branch_prefix, modal.branch, labels.branch_suffix
            ));
            ui.small(format!(
                "{} {}",
                labels.repo_hint,
                modal.repo_path.display()
            ));
            ui.add_space(8.0);

            if accounts.is_empty() {
                ui.weak(labels.no_accounts);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button(labels.close).clicked() {
                        intent.close = true;
                    }
                    if ui.button(labels.open_settings).clicked() {
                        intent.open_settings = true;
                    }
                });
                return;
            }

            let selected_missing = modal
                .selected_account
                .as_ref()
                .map(|selected| accounts.iter().all(|account| account.id != *selected))
                .unwrap_or(true);
            if selected_missing {
                modal.selected_account = accounts.first().map(|account| account.id.clone());
                modal.owners.clear();
                modal.selected_owner = None;
                modal.last_error = None;
                if modal.owners_task.is_none() {
                    intent.load_owners = modal.selected_account.clone();
                }
            }

            ui.horizontal(|ui| {
                ui.label(labels.account);
                let selected_label = modal
                    .selected_account
                    .as_ref()
                    .and_then(|selected| accounts.iter().find(|account| account.id == *selected))
                    .map(account_label)
                    .unwrap_or_else(|| labels.choose_account.to_string());
                egui::ComboBox::from_id_salt("publish_remote_account")
                    .selected_text(selected_label)
                    .width(260.0)
                    .show_ui(ui, |ui| {
                        for account in &accounts {
                            let selected = modal.selected_account.as_ref() == Some(&account.id);
                            if ui
                                .selectable_label(selected, account_label(account))
                                .clicked()
                            {
                                modal.selected_account = Some(account.id.clone());
                                modal.owners.clear();
                                modal.selected_owner = None;
                                modal.last_error = None;
                                intent.load_owners = Some(account.id.clone());
                            }
                        }
                    });
            });

            if let Some(err) = &modal.last_error {
                ui.add_space(6.0);
                ui.colored_label(egui::Color32::LIGHT_RED, err);
            }

            if modal.owners_task.is_some() && modal.owners.is_empty() {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(labels.loading_owners);
                });
                return;
            }

            if modal.owners.is_empty() {
                ui.add_space(8.0);
                ui.weak(labels.no_owners);
                return;
            }

            if modal
                .selected_owner
                .as_ref()
                .is_none_or(|login| !modal.owners.iter().any(|owner| owner.login == *login))
            {
                modal.selected_owner = modal.owners.first().map(|owner| owner.login.clone());
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(labels.owner);
                let owner_text = modal
                    .selected_owner
                    .as_deref()
                    .and_then(|login| modal.owners.iter().find(|owner| owner.login == login))
                    .map(owner_label)
                    .unwrap_or_else(|| labels.choose_owner.to_string());
                egui::ComboBox::from_id_salt("publish_remote_owner")
                    .selected_text(owner_text)
                    .width(260.0)
                    .show_ui(ui, |ui| {
                        for owner in &modal.owners {
                            let selected = modal.selected_owner.as_ref() == Some(&owner.login);
                            if ui.selectable_label(selected, owner_label(owner)).clicked() {
                                modal.selected_owner = Some(owner.login.clone());
                            }
                        }
                    });
            });

            ui.horizontal(|ui| {
                ui.label(labels.repo_name);
                ui.text_edit_singleline(&mut modal.repository_name);
            });
            ui.horizontal(|ui| {
                ui.label(labels.remote_name);
                ui.text_edit_singleline(&mut modal.remote_name);
            });
            ui.horizontal(|ui| {
                ui.label(labels.description);
                ui.text_edit_singleline(&mut modal.description);
            });
            ui.checkbox(&mut modal.private, labels.private_repo);
            ui.small(labels.https_hint);
            ui.small(labels.no_auto_init_hint);

            if modal.create_task.is_some() {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(labels.creating);
                });
            }

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(labels.close).clicked() {
                    intent.close = true;
                }

                let can_submit = !active_repo_job
                    && modal.create_task.is_none()
                    && modal.selected_account.is_some()
                    && modal.selected_owner.is_some()
                    && !modal.repository_name.trim().is_empty()
                    && !modal.remote_name.trim().is_empty();
                ui.add_enabled_ui(can_submit, |ui| {
                    if ui.button(labels.submit).clicked() {
                        let Some(account) = modal.selected_account.clone() else {
                            return;
                        };
                        let Some(owner) = modal.selected_owner.as_ref().and_then(|login| {
                            modal.owners.iter().find(|owner| owner.login == *login)
                        }) else {
                            return;
                        };
                        intent.create = Some((
                            account,
                            CreateRepositoryDraft {
                                owner: owner.login.clone(),
                                owner_kind: owner.kind,
                                name: modal.repository_name.trim().to_string(),
                                description: (!modal.description.trim().is_empty())
                                    .then(|| modal.description.trim().to_string()),
                                private: modal.private,
                                auto_init: false,
                            },
                        ));
                    }
                });
            });

            if active_repo_job {
                ui.add_space(6.0);
                ui.weak(labels.wait_for_job);
            }
        });

    if intent.close {
        app.publish_remote_modal = None;
    }
    if intent.open_settings {
        app.open_settings();
    }
    if let Some(account_id) = intent.load_owners {
        if let Some(account) = accounts
            .iter()
            .find(|account| account.id == account_id)
            .cloned()
        {
            app.load_publish_remote_owners(&account);
        }
    }
    if let Some((account_id, draft)) = intent.create {
        if let Some(account) = accounts
            .iter()
            .find(|account| account.id == account_id)
            .cloned()
        {
            app.create_publish_remote(&account, draft);
        }
    }
}

fn account_label(account: &ProviderAccount) -> String {
    format!("{} ({})", account.display_name, provider_label(account))
}

fn provider_label(account: &ProviderAccount) -> String {
    match &account.id.kind {
        crate::providers::ProviderKind::GitHub => "GitHub".to_string(),
        crate::providers::ProviderKind::GitLab => "GitLab".to_string(),
        crate::providers::ProviderKind::Bitbucket => "Bitbucket".to_string(),
        crate::providers::ProviderKind::AzureDevOps => "Azure DevOps".to_string(),
        crate::providers::ProviderKind::Codeberg => "Codeberg".to_string(),
        crate::providers::ProviderKind::Gitea { instance } => instance.clone(),
        crate::providers::ProviderKind::Generic { host } => host.clone(),
    }
}

fn owner_label(owner: &RemoteRepoOwner) -> String {
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

struct Labels {
    title: &'static str,
    branch_prefix: &'static str,
    branch_suffix: &'static str,
    repo_hint: &'static str,
    no_accounts: &'static str,
    open_settings: &'static str,
    close: &'static str,
    account: &'static str,
    choose_account: &'static str,
    loading_owners: &'static str,
    no_owners: &'static str,
    owner: &'static str,
    choose_owner: &'static str,
    repo_name: &'static str,
    remote_name: &'static str,
    description: &'static str,
    private_repo: &'static str,
    https_hint: &'static str,
    no_auto_init_hint: &'static str,
    creating: &'static str,
    submit: &'static str,
    wait_for_job: &'static str,
}

fn labels(language: UiLanguage) -> Labels {
    match language {
        UiLanguage::Korean => Labels {
            title: "원격 저장소 만들고 Publish",
            branch_prefix: "현재 브랜치",
            branch_suffix: "를 새 원격 저장소로 publish합니다.",
            repo_hint: "대상 저장소:",
            no_accounts: "먼저 설정 → 연동에서 Git 호스트 계정을 연결해야 합니다.",
            open_settings: "설정 열기",
            close: "닫기",
            account: "계정:",
            choose_account: "계정 선택",
            loading_owners: "계정/조직 목록을 불러오는 중…",
            no_owners: "이 계정으로 생성할 수 있는 owner가 없습니다.",
            owner: "소유자:",
            choose_owner: "소유자 선택",
            repo_name: "저장소 이름:",
            remote_name: "로컬 remote 이름:",
            description: "설명:",
            private_repo: "비공개 저장소",
            https_hint: "생성된 remote는 HTTPS로 추가해서 연결된 계정으로 바로 push 되게 합니다.",
            no_auto_init_hint: "README 초기화는 끄고 생성합니다. 기존 로컬 히스토리를 바로 publish하기 위해서입니다.",
            creating: "원격 저장소를 만드는 중…",
            submit: "원격 만들고 Push",
            wait_for_job: "다른 git 작업이 끝난 뒤 다시 시도할 수 있습니다.",
        },
        _ => Labels {
            title: "Create Remote And Publish",
            branch_prefix: "Publish current branch",
            branch_suffix: "to a new hosted repository.",
            repo_hint: "Local repository:",
            no_accounts: "Connect a Git host account in Settings → Integrations first.",
            open_settings: "Open settings",
            close: "Close",
            account: "Account:",
            choose_account: "Choose account",
            loading_owners: "Loading accounts and organizations…",
            no_owners: "No repository owners are available for this account.",
            owner: "Owner:",
            choose_owner: "Choose owner",
            repo_name: "Repository name:",
            remote_name: "Local remote name:",
            description: "Description:",
            private_repo: "Private repository",
            https_hint: "The created remote is added as HTTPS so the connected account can push without SSH setup.",
            no_auto_init_hint: "README auto-init stays off here so your existing local history can publish cleanly.",
            creating: "Creating remote repository…",
            submit: "Create remote and push",
            wait_for_job: "Wait for the current git job to finish first.",
        },
    }
}
