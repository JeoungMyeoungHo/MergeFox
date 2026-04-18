use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use egui::{ComboBox, RichText, TextEdit};
use secrecy::SecretString;

use super::{persist_config, Feedback};
use crate::app::MergeFoxApp;
use crate::config::{Config, UiLanguage};
use crate::providers::{self, AccountId, AuthMethod, ProviderAccount, ProviderKind};

pub struct IntegrationsDraft {
    pub selected: ProviderTarget,
    pub username: String,
    pub pat_token: String,
    pub oauth_client_id: String,
    pub gitea_instance: String,
    pub generic_host: String,
    pub oauth: OAuthDraft,
}

#[derive(Debug, Clone, Default)]
pub struct OAuthDraft {
    pub phase: OAuthPhase,
}

impl OAuthDraft {
    fn reset(&mut self) {
        self.phase = OAuthPhase::Idle;
    }
}

#[derive(Debug, Clone, Default)]
pub enum OAuthPhase {
    #[default]
    Idle,
    Starting,
    WaitingApproval(OAuthPendingFlow),
}

#[derive(Debug, Clone)]
pub struct OAuthPendingFlow {
    pub kind: ProviderKind,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
}

pub struct OAuthStartOutcome {
    pub kind: ProviderKind,
    pub config: crate::providers::OAuthDeviceConfig,
    pub device: crate::providers::oauth::DeviceCodeResponse,
}

pub struct OAuthConnectOutcome {
    pub account: ProviderAccount,
    pub access_token: SecretString,
}

impl IntegrationsDraft {
    pub fn from_config(config: &Config) -> Self {
        let selected = config
            .provider_accounts
            .first()
            .map(|account| ProviderTarget::from_kind(&account.id.kind))
            .unwrap_or(ProviderTarget::GitHub);
        let mut draft = Self {
            selected,
            username: String::new(),
            pat_token: String::new(),
            oauth_client_id: String::new(),
            gitea_instance: "https://git.example.com".to_string(),
            generic_host: "git.example.com".to_string(),
            oauth: OAuthDraft::default(),
        };
        if let Some(account) = config
            .provider_accounts
            .iter()
            .find(|account| ProviderTarget::from_kind(&account.id.kind) == selected)
        {
            draft.username = account.id.username.clone();
            match &account.id.kind {
                ProviderKind::Gitea { instance } => draft.gitea_instance = instance.clone(),
                ProviderKind::Generic { host } => draft.generic_host = host.clone(),
                _ => {}
            }
        }
        draft
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderTarget {
    GitHub,
    GitLab,
    Codeberg,
    Bitbucket,
    AzureDevOps,
    Gitea,
    Generic,
}

impl ProviderTarget {
    fn all() -> &'static [Self] {
        &[
            Self::GitHub,
            Self::GitLab,
            Self::Codeberg,
            Self::Bitbucket,
            Self::AzureDevOps,
            Self::Gitea,
            Self::Generic,
        ]
    }

    fn from_kind(kind: &ProviderKind) -> Self {
        match kind {
            ProviderKind::GitHub => Self::GitHub,
            ProviderKind::GitLab => Self::GitLab,
            ProviderKind::Codeberg => Self::Codeberg,
            ProviderKind::Bitbucket => Self::Bitbucket,
            ProviderKind::AzureDevOps => Self::AzureDevOps,
            ProviderKind::Gitea { .. } => Self::Gitea,
            ProviderKind::Generic { .. } => Self::Generic,
        }
    }

    fn label(self, lang: UiLanguage) -> &'static str {
        match (self, lang) {
            (Self::GitHub, _) => "GitHub",
            (Self::GitLab, _) => "GitLab",
            (Self::Codeberg, _) => "Codeberg",
            (Self::Bitbucket, _) => "Bitbucket",
            (Self::AzureDevOps, _) => "Azure DevOps",
            (Self::Gitea, UiLanguage::Korean) => "Gitea / 자체 호스팅",
            (Self::Gitea, _) => "Gitea / Self-hosted",
            (Self::Generic, UiLanguage::Korean) => "일반 Git 호스트",
            (Self::Generic, _) => "Generic Git Host",
        }
    }

    fn heading(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
            Self::GitLab => "GitLab",
            Self::Codeberg => "Codeberg",
            Self::Bitbucket => "Bitbucket",
            Self::AzureDevOps => "Azure DevOps",
            Self::Gitea => "Gitea",
            Self::Generic => "Generic Host",
        }
    }

    fn supports_pat(self) -> bool {
        true
    }

    fn supports_oauth(self) -> bool {
        matches!(self, Self::GitHub | Self::Codeberg | Self::Gitea)
    }

    fn kind(self, draft: &IntegrationsDraft) -> anyhow::Result<ProviderKind> {
        use anyhow::{bail, Context};

        Ok(match self {
            Self::GitHub => ProviderKind::GitHub,
            Self::GitLab => ProviderKind::GitLab,
            Self::Codeberg => ProviderKind::Codeberg,
            Self::Bitbucket => ProviderKind::Bitbucket,
            Self::AzureDevOps => ProviderKind::AzureDevOps,
            Self::Gitea => {
                let instance = draft.gitea_instance.trim();
                if instance.is_empty() {
                    bail!("enter the Gitea/Forgejo instance URL first");
                }
                let parsed = url::Url::parse(instance).context("parse instance URL")?;
                let host = parsed
                    .host_str()
                    .context("instance URL is missing a host")?;
                ProviderKind::Gitea {
                    instance: format!("{}://{}", parsed.scheme(), host),
                }
            }
            Self::Generic => {
                let host = draft.generic_host.trim();
                if host.is_empty() {
                    bail!("enter the host name first");
                }
                ProviderKind::Generic {
                    host: host.to_string(),
                }
            }
        })
    }

    fn oauth_config(
        self,
        draft: &IntegrationsDraft,
    ) -> anyhow::Result<(ProviderKind, crate::providers::OAuthDeviceConfig)> {
        use anyhow::bail;

        match self {
            Self::GitHub => Ok((
                ProviderKind::GitHub,
                crate::providers::oauth::github_device_config(),
            )),
            Self::Codeberg => {
                let client_id = draft.oauth_client_id.trim();
                if client_id.is_empty() {
                    bail!("enter the OAuth client ID first");
                }
                Ok((
                    ProviderKind::Codeberg,
                    crate::providers::oauth::gitea_device_config(
                        "https://codeberg.org",
                        client_id.to_string(),
                    ),
                ))
            }
            Self::Gitea => {
                let instance = draft.gitea_instance.trim();
                if instance.is_empty() {
                    bail!("enter the Gitea/Forgejo instance URL first");
                }
                let client_id = draft.oauth_client_id.trim();
                if client_id.is_empty() {
                    bail!("enter the OAuth client ID first");
                }
                Ok((
                    self.kind(draft)?,
                    crate::providers::oauth::gitea_device_config(instance, client_id.to_string()),
                ))
            }
            Self::GitLab | Self::Bitbucket | Self::AzureDevOps | Self::Generic => {
                let _ = draft;
                bail!("OAuth device flow is not implemented for this provider yet")
            }
        }
    }

    fn matches_kind(self, kind: &ProviderKind) -> bool {
        matches!(
            (self, kind),
            (Self::GitHub, ProviderKind::GitHub)
                | (Self::GitLab, ProviderKind::GitLab)
                | (Self::Codeberg, ProviderKind::Codeberg)
                | (Self::Bitbucket, ProviderKind::Bitbucket)
                | (Self::AzureDevOps, ProviderKind::AzureDevOps)
                | (Self::Gitea, ProviderKind::Gitea { .. })
                | (Self::Generic, ProviderKind::Generic { .. })
        )
    }

    fn ssh_slug(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::GitLab => "gitlab",
            Self::Codeberg => "codeberg",
            Self::Bitbucket => "bitbucket",
            Self::AzureDevOps => "azure-devops",
            Self::Gitea => "gitea",
            Self::Generic => "generic",
        }
    }

    fn ssh_settings_url(self, draft: &IntegrationsDraft) -> Option<String> {
        match self {
            Self::GitHub => Some("https://github.com/settings/keys".to_string()),
            Self::GitLab => Some("https://gitlab.com/-/user_settings/ssh_keys".to_string()),
            Self::Codeberg => Some("https://codeberg.org/user/settings/keys".to_string()),
            Self::Bitbucket => Some("https://bitbucket.org/account/settings/ssh-keys/".to_string()),
            Self::AzureDevOps => None,
            Self::Gitea => {
                let instance = draft.gitea_instance.trim().trim_end_matches('/');
                if instance.is_empty() {
                    None
                } else {
                    Some(format!("{instance}/user/settings/keys"))
                }
            }
            Self::Generic => None,
        }
    }
}

pub fn show(ui: &mut egui::Ui, app: &mut MergeFoxApp) {
    let language = current_language(app);
    let labels = labels(language);
    poll_oauth(app, &labels);
    let selected = app
        .settings_modal
        .as_ref()
        .map(|modal| modal.integrations.selected)
        .unwrap_or(ProviderTarget::GitHub);
    let accounts = app
        .config
        .provider_accounts
        .iter()
        .filter(|account| selected.matches_kind(&account.id.kind))
        .cloned()
        .collect::<Vec<_>>();
    let ssh_keys = providers::ssh::list_existing_keys();

    let mut intent: Option<Intent> = None;

    ui.heading(labels.heading);
    ui.separator();
    ui.weak(labels.intro);
    ui.add_space(8.0);

    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(220.0, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| render_provider_list(ui, app, &labels),
        );
        ui.separator();
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                render_provider_body(ui, app, &labels, &accounts, &ssh_keys, &mut intent);
            },
        );
    });

    if let Some(intent) = intent {
        handle_intent(app, ui.ctx(), intent, &labels);
    }
}

fn render_provider_list(ui: &mut egui::Ui, app: &mut MergeFoxApp, labels: &Labels) {
    ui.heading(labels.providers);
    ui.add_space(6.0);

    for provider in ProviderTarget::all() {
        let Some(modal) = app.settings_modal.as_mut() else {
            return;
        };
        let selected = modal.integrations.selected == *provider;
        if ui
            .selectable_label(selected, provider.label(labels.lang))
            .clicked()
        {
            modal.integrations.selected = *provider;
            modal.integrations.oauth.reset();
            modal.feedback = None;
            app.provider_oauth_start_task = None;
            app.provider_oauth_poll_task = None;
        }
    }
}

fn render_provider_body(
    ui: &mut egui::Ui,
    app: &mut MergeFoxApp,
    labels: &Labels,
    accounts: &[ProviderAccount],
    ssh_keys: &[PathBuf],
    intent: &mut Option<Intent>,
) {
    let oauth_starting = app.provider_oauth_start_task.is_some();
    let oauth_polling = app.provider_oauth_poll_task.is_some();
    let Some(modal) = app.settings_modal.as_mut() else {
        return;
    };
    let draft = &mut modal.integrations;

    ui.heading(draft.selected.heading());
    ui.separator();

    if matches!(draft.selected, ProviderTarget::GitHub) {
        ui.group(|ui| {
            ui.label(RichText::new(labels.scopes_heading).strong());
            ui.small(labels.scopes_intro);
            ui.small("`public_repo` - create PRs on public repositories");
            ui.small("`repo` - required for private repositories, issues, and templates");
            ui.small("`read:user` - show the connected account name and avatar");
            ui.small("`workflow` / `admin:org` - not requested");
        });
        ui.add_space(8.0);
    }

    if accounts.is_empty() {
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(labels.not_connected).color(egui::Color32::LIGHT_RED));
                ui.weak(labels.not_connected_hint);
            });
        });
    } else {
        ui.heading(labels.connected_accounts);
        ui.add_space(6.0);
        for account in accounts {
            let token_present = providers::pat::load_pat(&account.id)
                .ok()
                .flatten()
                .is_some();
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&account.display_name).strong());
                    let status = if token_present {
                        RichText::new(labels.connected).color(egui::Color32::LIGHT_GREEN)
                    } else {
                        RichText::new(labels.key_missing).color(egui::Color32::YELLOW)
                    };
                    ui.label(status);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(
                                RichText::new(labels.disconnect).color(egui::Color32::LIGHT_RED),
                            )
                            .clicked()
                        {
                            *intent = Some(Intent::Disconnect(account.id.clone()));
                        }
                    });
                });
                ui.small(format!(
                    "{}: {}",
                    labels.auth_method,
                    auth_method_label(account.method, labels.lang)
                ));
                ui.small(format!("{}: {}", labels.account_id, account.id.username));
                ui.small(format!(
                    "{}: {}",
                    labels.provider_label,
                    provider_host_label(&account.id.kind)
                ));
                ui.add_space(4.0);
                let before_ssh_key = account.ssh_key_path.clone();
                let mut selected_ssh_key = before_ssh_key.clone();
                let selected_ssh_label = selected_ssh_key
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| labels.auto_ssh_key.to_string());
                ui.horizontal(|ui| {
                    ui.small(format!("{}:", labels.bound_ssh_key));
                    ComboBox::from_id_salt(("settings_account_ssh_key", account.id.slug()))
                        .selected_text(selected_ssh_label)
                        .width(320.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut selected_ssh_key, None, labels.auto_ssh_key);
                            ui.separator();
                            for key_path in ssh_keys {
                                ui.selectable_value(
                                    &mut selected_ssh_key,
                                    Some(key_path.clone()),
                                    key_path.display().to_string(),
                                );
                            }
                        });
                    if let Some(path) = selected_ssh_key.as_ref() {
                        if public_key_path(path).exists()
                            && ui.small_button(labels.copy_public_key).clicked()
                        {
                            *intent = Some(Intent::CopyPublicKey(path.clone()));
                        }
                    }
                });
                if selected_ssh_key != before_ssh_key {
                    *intent = Some(Intent::BindSshKey {
                        account_id: account.id.clone(),
                        ssh_key_path: selected_ssh_key,
                    });
                }
                if let Some(path) = account.ssh_key_path.as_ref() {
                    if !path.exists() {
                        ui.colored_label(egui::Color32::YELLOW, labels.bound_ssh_key_missing);
                    }
                }
            });
            ui.add_space(6.0);
        }
    }

    ui.add_space(10.0);
    ui.heading(labels.connect_oauth);
    ui.separator();
    if draft.selected.supports_oauth() {
        ui.weak(labels.oauth_note);
        ui.add_space(6.0);
        if matches!(
            draft.selected,
            ProviderTarget::Codeberg | ProviderTarget::Gitea
        ) {
            ui.weak(labels.oauth_client_id_note);
            ui.add_space(6.0);
            field(ui, labels.oauth_client_id, |ui| {
                ui.add(
                    TextEdit::singleline(&mut draft.oauth_client_id)
                        .desired_width(f32::INFINITY)
                        .hint_text(labels.oauth_client_id_hint),
                );
            });
        }
        match &draft.oauth.phase {
            OAuthPhase::Idle => {
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !oauth_starting && !oauth_polling,
                            egui::Button::new(labels.start_oauth),
                        )
                        .clicked()
                    {
                        *intent = Some(Intent::ConnectOauth);
                    }
                    ui.weak(labels.oauth_supported_hint);
                });
            }
            OAuthPhase::Starting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak(labels.oauth_starting);
                    if ui.button(labels.oauth_cancel).clicked() {
                        *intent = Some(Intent::CancelOauth);
                    }
                });
            }
            OAuthPhase::WaitingApproval(flow) => {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(labels.oauth_waiting).strong());
                        if oauth_polling {
                            ui.spinner();
                        }
                    });
                    ui.small(format!(
                        "{}: {}",
                        labels.provider_label,
                        provider_host_label(&flow.kind)
                    ));
                    field(ui, labels.oauth_user_code, |ui| {
                        ui.monospace(&flow.user_code);
                    });
                    let link = flow
                        .verification_uri_complete
                        .as_deref()
                        .unwrap_or(&flow.verification_uri);
                    field(ui, labels.oauth_verification_url, |ui| {
                        ui.hyperlink_to(link, link);
                    });
                    ui.horizontal(|ui| {
                        if ui.button(labels.oauth_copy_code).clicked() {
                            ui.ctx().copy_text(flow.user_code.clone());
                            modal.feedback = Some(Feedback::ok(labels.oauth_code_copied));
                        }
                        if ui.button(labels.oauth_copy_link).clicked() {
                            ui.ctx().copy_text(link.to_string());
                            modal.feedback = Some(Feedback::ok(labels.oauth_link_copied));
                        }
                        if ui.button(labels.oauth_cancel).clicked() {
                            *intent = Some(Intent::CancelOauth);
                        }
                    });
                    ui.weak(labels.oauth_browser_hint);
                });
            }
        }
    } else {
        ui.weak(labels.oauth_unsupported);
    }

    ui.add_space(10.0);
    ui.heading(labels.connect_pat);
    ui.separator();
    ui.weak(labels.keyring_note);
    ui.add_space(6.0);

    if matches!(draft.selected, ProviderTarget::Gitea) {
        field(ui, labels.instance_url, |ui| {
            ui.add(
                TextEdit::singleline(&mut draft.gitea_instance)
                    .desired_width(f32::INFINITY)
                    .hint_text("https://git.example.com"),
            );
        });
    }
    if matches!(draft.selected, ProviderTarget::Generic) {
        field(ui, labels.host_name, |ui| {
            ui.add(
                TextEdit::singleline(&mut draft.generic_host)
                    .desired_width(f32::INFINITY)
                    .hint_text("git.example.com"),
            );
        });
    }

    field(ui, labels.username, |ui| {
        ui.add(
            TextEdit::singleline(&mut draft.username)
                .desired_width(f32::INFINITY)
                .hint_text(labels.username_hint),
        );
    });
    field(ui, labels.pat, |ui| {
        ui.add(
            TextEdit::singleline(&mut draft.pat_token)
                .desired_width(f32::INFINITY)
                .password(true)
                .hint_text(labels.pat_hint),
        );
    });

    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                draft.selected.supports_pat(),
                egui::Button::new(labels.connect),
            )
            .clicked()
        {
            *intent = Some(Intent::ConnectPat);
        }

        if let Ok(kind) = draft.selected.kind(draft) {
            ui.hyperlink_to(labels.open_pat_help, providers::pat::pat_help_url(&kind));
        } else {
            ui.weak(labels.enter_host_first);
        }
    });

    ui.add_space(10.0);
    ui.heading(labels.ssh_keys);
    ui.separator();
    ui.weak(labels.ssh_detected_note);
    ui.add_space(4.0);
    if ssh_keys.is_empty() {
        ui.weak(labels.no_ssh_keys);
    } else {
        for key_path in ssh_keys {
            ui.horizontal(|ui| {
                ui.monospace(key_path.display().to_string());
                if public_key_path(key_path).exists() && ui.button(labels.copy_public_key).clicked()
                {
                    *intent = Some(Intent::CopyPublicKey(key_path.clone()));
                }
            });
        }
    }
    let ssh_settings_url = draft.selected.ssh_settings_url(draft);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.button(labels.generate_ssh_key).clicked() {
            *intent = Some(Intent::GenerateSshKey {
                target: draft.selected,
                open_settings_url: None,
            });
        }
        if let Some(url) = ssh_settings_url.as_ref() {
            if ui.button(labels.generate_ssh_key_and_open).clicked() {
                *intent = Some(Intent::GenerateSshKey {
                    target: draft.selected,
                    open_settings_url: Some(url.clone()),
                });
            }
            if ui.button(labels.open_ssh_settings).clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
        }
    });
    ui.weak(labels.ssh_hint);
}

enum Intent {
    ConnectOauth,
    ConnectPat,
    CancelOauth,
    Disconnect(AccountId),
    BindSshKey {
        account_id: AccountId,
        ssh_key_path: Option<PathBuf>,
    },
    GenerateSshKey {
        target: ProviderTarget,
        open_settings_url: Option<String>,
    },
    CopyPublicKey(PathBuf),
}

fn handle_intent(app: &mut MergeFoxApp, ctx: &egui::Context, intent: Intent, labels: &Labels) {
    match intent {
        Intent::ConnectOauth => start_oauth(app, labels),
        Intent::ConnectPat => connect_pat(app, labels),
        Intent::CancelOauth => cancel_oauth(app),
        Intent::Disconnect(id) => disconnect(app, &id, labels),
        Intent::BindSshKey {
            account_id,
            ssh_key_path,
        } => bind_ssh_key(app, &account_id, ssh_key_path, labels),
        Intent::GenerateSshKey {
            target,
            open_settings_url,
        } => generate_ssh_key(app, ctx, target, open_settings_url.as_deref(), labels),
        Intent::CopyPublicKey(path) => copy_public_key(app, ctx, &path, labels),
    }
}

fn start_oauth(app: &mut MergeFoxApp, labels: &Labels) {
    let (kind, config) = {
        let Some(modal) = app.settings_modal.as_mut() else {
            return;
        };
        let draft = &mut modal.integrations;
        let (kind, config) = match draft.selected.oauth_config(draft) {
            Ok(ok) => ok,
            Err(err) => {
                modal.feedback = Some(Feedback::err(format!("{err:#}")));
                return;
            }
        };
        draft.oauth.phase = OAuthPhase::Starting;
        modal.feedback = Some(Feedback::ok(labels.oauth_starting));
        (kind, config)
    };

    app.provider_oauth_poll_task = None;
    app.provider_oauth_start_task =
        Some(crate::providers::runtime::ProviderTask::spawn(async move {
            let http = crate::providers::default_http_client();
            let device = crate::providers::oauth::start_device_flow(&config, &http).await?;
            Ok(OAuthStartOutcome {
                kind,
                config,
                device,
            })
        }));
}

fn cancel_oauth(app: &mut MergeFoxApp) {
    app.provider_oauth_start_task = None;
    app.provider_oauth_poll_task = None;
    if let Some(modal) = app.settings_modal.as_mut() {
        modal.integrations.oauth.reset();
        modal.feedback = None;
    }
}

fn poll_oauth(app: &mut MergeFoxApp, labels: &Labels) {
    poll_oauth_start(app, labels);
    poll_oauth_completion(app, labels);
}

fn poll_oauth_start(app: &mut MergeFoxApp, labels: &Labels) {
    let Some(task) = app.provider_oauth_start_task.as_mut() else {
        return;
    };
    let Some(result) = task.poll() else {
        return;
    };
    app.provider_oauth_start_task = None;

    match result {
        Ok(started) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.integrations.oauth.phase = OAuthPhase::WaitingApproval(OAuthPendingFlow {
                    kind: started.kind.clone(),
                    user_code: started.device.user_code.clone(),
                    verification_uri: started.device.verification_uri.clone(),
                    verification_uri_complete: started.device.verification_uri_complete.clone(),
                });
                modal.feedback = Some(Feedback::ok(labels.oauth_ready));
            }
            start_oauth_poll(app, started);
        }
        Err(err) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.integrations.oauth.reset();
                modal.feedback = Some(Feedback::err(format!("oauth: {err}")));
            }
        }
    }
}

fn start_oauth_poll(app: &mut MergeFoxApp, started: OAuthStartOutcome) {
    let OAuthStartOutcome {
        kind,
        config,
        device,
    } = started;
    app.provider_oauth_poll_task =
        Some(crate::providers::runtime::ProviderTask::spawn(async move {
            let http = crate::providers::default_http_client();
            let token = crate::providers::oauth::poll_token(
                &config,
                &device.device_code,
                Duration::from_secs(device.interval.max(1)),
                &http,
            )
            .await?;

            let provider = crate::providers::build(&kind).await;
            let profile = provider.current_user(&http, &token.access_token).await?;
            Ok(OAuthConnectOutcome {
                account: ProviderAccount {
                    id: AccountId {
                        kind,
                        username: profile.username.clone(),
                    },
                    display_name: profile.display_name,
                    avatar_url: profile.avatar_url,
                    method: AuthMethod::OAuth,
                    created_unix: unix_now() as i64,
                    ssh_key_path: None,
                },
                access_token: token.access_token,
            })
        }));
}

fn poll_oauth_completion(app: &mut MergeFoxApp, labels: &Labels) {
    let Some(task) = app.provider_oauth_poll_task.as_mut() else {
        return;
    };
    let Some(result) = task.poll() else {
        return;
    };
    app.provider_oauth_poll_task = None;

    match result {
        Ok(done) => {
            if let Err(err) = crate::providers::pat::store_pat(&done.account.id, done.access_token)
            {
                if let Some(modal) = app.settings_modal.as_mut() {
                    modal.integrations.oauth.reset();
                    modal.feedback = Some(Feedback::err(format!("keyring: {err:#}")));
                }
                return;
            }
            let username = done.account.id.username.clone();
            app.config.upsert_provider_account(done.account);
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.integrations.username = username;
                modal.integrations.oauth.reset();
            }
            persist_config(app, labels.oauth_connected_saved);
        }
        Err(err) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.integrations.oauth.reset();
                modal.feedback = Some(Feedback::err(format!("oauth: {err}")));
            }
        }
    }
}

fn connect_pat(app: &mut MergeFoxApp, labels: &Labels) {
    let (kind, username, token) = {
        let Some(modal) = app.settings_modal.as_ref() else {
            return;
        };
        let draft = &modal.integrations;
        if draft.username.trim().is_empty() {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::err(labels.err_username));
            }
            return;
        }
        if draft.pat_token.trim().is_empty() {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::err(labels.err_pat));
            }
            return;
        }
        let kind = match draft.selected.kind(draft) {
            Ok(kind) => kind,
            Err(err) => {
                if let Some(modal) = app.settings_modal.as_mut() {
                    modal.feedback = Some(Feedback::err(format!("{err:#}")));
                }
                return;
            }
        };
        (
            kind,
            draft.username.trim().to_string(),
            draft.pat_token.trim().to_string(),
        )
    };

    let id = AccountId {
        kind,
        username: username.clone(),
    };
    if let Err(err) = providers::pat::store_pat(&id, SecretString::new(token)) {
        if let Some(modal) = app.settings_modal.as_mut() {
            modal.feedback = Some(Feedback::err(format!("keyring: {err:#}")));
        }
        return;
    }

    app.config.upsert_provider_account(ProviderAccount {
        id: id.clone(),
        display_name: username,
        avatar_url: None,
        method: AuthMethod::Pat,
        created_unix: unix_now() as i64,
        ssh_key_path: None,
    });
    persist_config(app, labels.connected_saved);
    if let Some(modal) = app.settings_modal.as_mut() {
        modal.integrations.pat_token.clear();
    }
}

fn disconnect(app: &mut MergeFoxApp, id: &AccountId, labels: &Labels) {
    if let Err(err) = providers::pat::delete_pat(id) {
        if let Some(modal) = app.settings_modal.as_mut() {
            modal.feedback = Some(Feedback::err(format!("keyring: {err:#}")));
        }
        return;
    }
    app.config.remove_provider_account(id);
    persist_config(app, labels.disconnected_saved);
}

fn bind_ssh_key(
    app: &mut MergeFoxApp,
    id: &AccountId,
    ssh_key_path: Option<PathBuf>,
    labels: &Labels,
) {
    let Some(account) = app
        .config
        .provider_accounts
        .iter_mut()
        .find(|account| &account.id == id)
    else {
        if let Some(modal) = app.settings_modal.as_mut() {
            modal.feedback = Some(Feedback::err(format!(
                "provider account not found: {}",
                id.slug()
            )));
        }
        return;
    };
    account.ssh_key_path = ssh_key_path;
    persist_config(app, labels.saved_ssh_binding);
}

fn generate_ssh_key(
    app: &mut MergeFoxApp,
    ctx: &egui::Context,
    target: ProviderTarget,
    open_settings_url: Option<&str>,
    labels: &Labels,
) {
    let path = allocate_ssh_key_path(target);
    let comment = format!("mergefox-{}-{}", target.ssh_slug(), unix_now());
    let generated = match providers::ssh::generate_ed25519(&comment) {
        Ok(generated) => generated,
        Err(err) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::err(format!("generate ssh key: {err:#}")));
            }
            return;
        }
    };

    match providers::ssh::save_key_pair(&generated, &path) {
        Ok(()) => {
            if open_settings_url.is_some() {
                ctx.copy_text(generated.public_openssh.clone());
            }
            if let Some(modal) = app.settings_modal.as_mut() {
                let message = if open_settings_url.is_some() {
                    format!(
                        "{}: {} ({})",
                        labels.generated_ssh_key,
                        path.display(),
                        labels.copied_public_key
                    )
                } else {
                    format!("{}: {}", labels.generated_ssh_key, path.display())
                };
                modal.feedback = Some(Feedback::ok(message));
            }
            app.hud = Some(crate::app::Hud::new(labels.generated_ssh_key, 1600));
            if let Some(url) = open_settings_url {
                ctx.open_url(egui::OpenUrl::new_tab(url));
            }
        }
        Err(err) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::err(format!("save ssh key: {err:#}")));
            }
        }
    }
}

fn copy_public_key(app: &mut MergeFoxApp, ctx: &egui::Context, path: &Path, labels: &Labels) {
    let pub_path = public_key_path(path);
    match fs::read_to_string(&pub_path) {
        Ok(text) => {
            ctx.copy_text(text);
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::ok(labels.copied_public_key));
            }
            app.hud = Some(crate::app::Hud::new(labels.copied_public_key, 1600));
        }
        Err(err) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::err(format!("read {}: {err}", pub_path.display())));
            }
        }
    }
}

fn field<R>(ui: &mut egui::Ui, label: &str, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.add_sized([160.0, 20.0], egui::Label::new(label));
        add_contents(ui)
    })
    .inner
}

fn provider_host_label(kind: &ProviderKind) -> String {
    match kind {
        ProviderKind::GitHub => "github.com".to_string(),
        ProviderKind::GitLab => "gitlab.com".to_string(),
        ProviderKind::Codeberg => "codeberg.org".to_string(),
        ProviderKind::Bitbucket => "bitbucket.org".to_string(),
        ProviderKind::AzureDevOps => "dev.azure.com".to_string(),
        ProviderKind::Gitea { instance } => instance.clone(),
        ProviderKind::Generic { host } => host.clone(),
    }
}

fn auth_method_label(method: AuthMethod, lang: UiLanguage) -> &'static str {
    match (method, lang) {
        (AuthMethod::Pat, UiLanguage::Korean) => "PAT",
        (AuthMethod::OAuth, UiLanguage::Korean) => "OAuth",
        (AuthMethod::Ssh, UiLanguage::Korean) => "SSH",
        (AuthMethod::Pat, _) => "PAT",
        (AuthMethod::OAuth, _) => "OAuth",
        (AuthMethod::Ssh, _) => "SSH",
    }
}

fn current_language(app: &MergeFoxApp) -> UiLanguage {
    app.settings_modal
        .as_ref()
        .map(|m| m.language.resolved())
        .unwrap_or_else(|| app.config.ui_language.resolved())
}

fn allocate_ssh_key_path(target: ProviderTarget) -> PathBuf {
    let base = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ssh");
    for suffix in 0..100u32 {
        let filename = if suffix == 0 {
            format!("mergefox_{}", target.ssh_slug())
        } else {
            format!("mergefox_{}_{}", target.ssh_slug(), suffix)
        };
        let path = base.join(filename);
        if !path.exists() && !public_key_path(&path).exists() {
            return path;
        }
    }
    base.join(format!("mergefox_{}_{}", target.ssh_slug(), unix_now()))
}

fn public_key_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".pub");
    PathBuf::from(s)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct Labels {
    lang: UiLanguage,
    heading: &'static str,
    intro: &'static str,
    providers: &'static str,
    not_connected: &'static str,
    not_connected_hint: &'static str,
    connected_accounts: &'static str,
    connected: &'static str,
    key_missing: &'static str,
    disconnect: &'static str,
    auth_method: &'static str,
    account_id: &'static str,
    provider_label: &'static str,
    scopes_heading: &'static str,
    scopes_intro: &'static str,
    connect_oauth: &'static str,
    oauth_note: &'static str,
    oauth_client_id_note: &'static str,
    oauth_client_id: &'static str,
    oauth_client_id_hint: &'static str,
    oauth_supported_hint: &'static str,
    oauth_unsupported: &'static str,
    start_oauth: &'static str,
    oauth_starting: &'static str,
    oauth_waiting: &'static str,
    oauth_user_code: &'static str,
    oauth_verification_url: &'static str,
    oauth_browser_hint: &'static str,
    oauth_copy_code: &'static str,
    oauth_copy_link: &'static str,
    oauth_cancel: &'static str,
    oauth_ready: &'static str,
    oauth_connected_saved: &'static str,
    oauth_code_copied: &'static str,
    oauth_link_copied: &'static str,
    connect_pat: &'static str,
    keyring_note: &'static str,
    instance_url: &'static str,
    host_name: &'static str,
    username: &'static str,
    username_hint: &'static str,
    pat: &'static str,
    pat_hint: &'static str,
    connect: &'static str,
    open_pat_help: &'static str,
    enter_host_first: &'static str,
    ssh_keys: &'static str,
    no_ssh_keys: &'static str,
    copy_public_key: &'static str,
    bound_ssh_key: &'static str,
    bound_ssh_key_missing: &'static str,
    auto_ssh_key: &'static str,
    generate_ssh_key: &'static str,
    generate_ssh_key_and_open: &'static str,
    open_ssh_settings: &'static str,
    ssh_hint: &'static str,
    ssh_detected_note: &'static str,
    connected_saved: &'static str,
    disconnected_saved: &'static str,
    generated_ssh_key: &'static str,
    copied_public_key: &'static str,
    saved_ssh_binding: &'static str,
    err_username: &'static str,
    err_pat: &'static str,
}

fn labels(lang: UiLanguage) -> Labels {
    match lang {
        UiLanguage::Korean => Labels {
            lang,
            heading: "연동",
            intro: "Git provider 계정을 연결해 두면 이후 PR/이슈/원격 작업 기능에 바로 재사용할 수 있습니다. 토큰은 설정 파일이 아니라 OS 키체인에 저장됩니다.",
            providers: "Provider",
            not_connected: "연결되지 않음",
            not_connected_hint: "아래에서 OAuth 또는 PAT를 연결하면 이 provider가 설정에 등록됩니다.",
            connected_accounts: "연결된 계정",
            connected: "연결됨",
            key_missing: "키체인에서 토큰 없음",
            disconnect: "연결 해제",
            auth_method: "인증 방식",
            account_id: "계정",
            provider_label: "호스트",
            scopes_heading: "GitHub 권한 안내",
            scopes_intro: "mergeFox는 필요한 권한만 요청합니다. 공개 저장소만 쓸 때는 `public_repo`, 비공개 저장소까지 포함하면 `repo`가 필요합니다.",
            connect_oauth: "OAuth 연결",
            oauth_note: "브라우저에서 승인하고 앱으로 돌아오는 device flow 방식입니다. GitHub는 바로 사용할 수 있고, Codeberg/Gitea는 직접 만든 OAuth 앱의 client ID가 필요합니다.",
            oauth_client_id_note: "Codeberg/Gitea OAuth는 인스턴스에 등록한 OAuth 앱의 client ID를 사용합니다.",
            oauth_client_id: "OAuth client ID",
            oauth_client_id_hint: "예: provider에서 발급한 public client ID",
            oauth_supported_hint: "GitHub는 바로 승인할 수 있고, Codeberg/Gitea는 client ID 입력 후 같은 흐름으로 연결됩니다.",
            oauth_unsupported: "이 provider는 아직 PAT 연결만 지원합니다. OAuth device flow는 현재 GitHub와 Gitea 계열에서만 제공합니다.",
            start_oauth: "OAuth 시작",
            oauth_starting: "OAuth device code를 요청하는 중입니다...",
            oauth_waiting: "브라우저에서 승인 대기 중",
            oauth_user_code: "사용자 코드",
            oauth_verification_url: "인증 URL",
            oauth_browser_hint: "위 URL을 열고 코드를 입력해 승인하면 연결이 자동으로 완료됩니다.",
            oauth_copy_code: "코드 복사",
            oauth_copy_link: "링크 복사",
            oauth_cancel: "취소",
            oauth_ready: "브라우저 승인 코드를 받았습니다",
            oauth_connected_saved: "OAuth 계정을 저장했습니다",
            oauth_code_copied: "OAuth 코드를 복사했습니다",
            oauth_link_copied: "OAuth 링크를 복사했습니다",
            connect_pat: "PAT 연결",
            keyring_note: "PAT/OAuth 토큰은 OS 키체인에만 저장됩니다. 현재는 GitHub OAuth, PAT 연결/해제, 로컬 SSH 키 관리를 지원합니다.",
            instance_url: "인스턴스 URL",
            host_name: "호스트 이름",
            username: "사용자명",
            username_hint: "예: provider 사용자명",
            pat: "Personal Access Token",
            pat_hint: "토큰을 붙여넣으면 저장 후 입력란은 비워집니다",
            connect: "연결",
            open_pat_help: "PAT 발급 페이지 열기",
            enter_host_first: "먼저 호스트 정보를 입력하세요.",
            ssh_keys: "SSH 키",
            ssh_detected_note: "아래 목록은 이 컴퓨터의 ~/.ssh 디렉터리에서 실제로 발견한 키 경로입니다.",
            no_ssh_keys: "발견된 로컬 SSH 키가 없습니다.",
            copy_public_key: "공개키 복사",
            bound_ssh_key: "바인딩된 SSH 키",
            bound_ssh_key_missing: "바인딩된 SSH 키 파일을 찾을 수 없습니다. 다른 키를 고르거나 경로를 확인하세요.",
            auto_ssh_key: "(바인딩 안 함)",
            generate_ssh_key: "mergeFox SSH 키 생성",
            generate_ssh_key_and_open: "생성 후 브라우저 열기",
            open_ssh_settings: "SSH 키 등록 페이지 열기",
            ssh_hint: "SSH remote(`git@...`)는 연결된 PAT/OAuth 계정을 사용하지 않습니다. 생성된 키는 ~/.ssh 아래에 저장되며, 공개키를 Git provider의 SSH keys 페이지에 등록해야 SSH push/pull에 사용할 수 있습니다.",
            connected_saved: "계정을 저장했습니다",
            disconnected_saved: "계정을 제거했습니다",
            generated_ssh_key: "SSH 키를 생성했습니다",
            copied_public_key: "공개키를 복사했습니다",
            saved_ssh_binding: "SSH 키 바인딩을 저장했습니다",
            err_username: "사용자명을 입력하세요.",
            err_pat: "PAT를 입력하세요.",
        },
        _ => Labels {
            lang,
            heading: "Integrations",
            intro: "Connect Git provider accounts here so future PR, issue, and remote workflows can reuse them. Tokens are stored only in the OS keychain.",
            providers: "Providers",
            not_connected: "Not connected",
            not_connected_hint: "Connect with OAuth or save a PAT below to register this provider in Settings.",
            connected_accounts: "Connected Accounts",
            connected: "Connected",
            key_missing: "Token missing from keychain",
            disconnect: "Disconnect",
            auth_method: "Auth",
            account_id: "Account",
            provider_label: "Host",
            scopes_heading: "GitHub scopes in use",
            scopes_intro: "mergeFox only asks for the scopes needed for PRs, issues, and profile lookup. Use `public_repo` for public-only access, or `repo` when private repositories are involved.",
            connect_oauth: "Connect with OAuth",
            oauth_note: "Uses browser-based device flow. GitHub works out of the box, while Codeberg/Gitea need the client ID from your own registered OAuth app.",
            oauth_client_id_note: "For Codeberg/Gitea, paste the client ID from the OAuth app registered on that instance.",
            oauth_client_id: "OAuth client ID",
            oauth_client_id_hint: "e.g. the public client ID from your provider",
            oauth_supported_hint: "Approve in the browser and mergeFox will finish automatically. Codeberg/Gitea need a client ID first.",
            oauth_unsupported: "This provider is PAT-only for now. OAuth device flow is currently available for GitHub and Gitea-family hosts.",
            start_oauth: "Start OAuth",
            oauth_starting: "Requesting an OAuth device code...",
            oauth_waiting: "Waiting for browser approval",
            oauth_user_code: "User code",
            oauth_verification_url: "Verification URL",
            oauth_browser_hint: "Open the URL above, enter the code, and approve access. The app will finish automatically.",
            oauth_copy_code: "Copy code",
            oauth_copy_link: "Copy link",
            oauth_cancel: "Cancel",
            oauth_ready: "Received the browser approval code",
            oauth_connected_saved: "Saved OAuth account",
            oauth_code_copied: "Copied OAuth code",
            oauth_link_copied: "Copied OAuth link",
            connect_pat: "Connect with PAT",
            keyring_note: "PAT and OAuth tokens are stored only in the OS keychain. This version supports GitHub OAuth, PAT connect/disconnect, and local SSH key management.",
            instance_url: "Instance URL",
            host_name: "Host name",
            username: "Username",
            username_hint: "e.g. your provider username",
            pat: "Personal Access Token",
            pat_hint: "The field is cleared after the token is saved",
            connect: "Connect",
            open_pat_help: "Open PAT help",
            enter_host_first: "Enter the host details first.",
            ssh_keys: "SSH Keys",
            ssh_detected_note: "The paths below are detected from this machine's ~/.ssh directory.",
            no_ssh_keys: "No local SSH keys were found.",
            copy_public_key: "Copy public key",
            bound_ssh_key: "Bound SSH key",
            bound_ssh_key_missing: "The bound SSH key file is missing. Pick another key or fix the path.",
            auto_ssh_key: "(no bound key)",
            generate_ssh_key: "Generate mergeFox SSH key",
            generate_ssh_key_and_open: "Generate + Open browser",
            open_ssh_settings: "Open SSH key settings",
            ssh_hint: "SSH remotes (`git@...`) do not use the connected PAT/OAuth accounts above. Generated keys are saved under ~/.ssh, and the public key must be added to your Git provider's SSH keys page before SSH pull/push will work.",
            connected_saved: "Saved provider account",
            disconnected_saved: "Removed provider account",
            generated_ssh_key: "Generated SSH key",
            copied_public_key: "Copied public key",
            saved_ssh_binding: "Saved SSH key binding",
            err_username: "Enter a username first.",
            err_pat: "Enter a PAT first.",
        },
    }
}
