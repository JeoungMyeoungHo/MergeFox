//! Welcome screen: unified input (URL / path / query) + Recent list.

use std::path::PathBuf;

use crate::app::{CloneSizePrompt, MergeFoxApp};
use crate::clone::{self, Stage};
use crate::config::{CloneSizePolicy, UiLanguage};
use crate::git_url;
use crate::providers::{self, AccountId, ProviderAccount, RemoteRepoSummary};

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
}

#[derive(Debug)]
enum CloneDecision {
    /// Spawn a full clone for the pending prompt, clear the prompt.
    Full,
    /// Spawn a shallow clone at the prompt's chosen depth.
    Shallow,
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
    if let Some(path) = intent.open_path {
        app.open_repo(&path);
    }
    if let Some(path) = intent.init_path {
        app.init_repo(&path);
    }
    if intent.open_settings {
        app.open_settings();
    }
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

    ui.horizontal(|ui| {
        let selected_label = selected_account
            .map(account_label)
            .unwrap_or_else(|| labels.choose_account.to_string());
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
                        intent.refresh_remote_repos = Some(account.id.clone());
                    }
                }
            });

        let refresh_clicked = ui
            .add_enabled(
                state.remote_repos.task.is_none() && selected_account.is_some(),
                egui::Button::new(labels.refresh_remote_repos),
            )
            .clicked();
        if refresh_clicked {
            if let Some(account) = selected_account {
                intent.refresh_remote_repos = Some(account.id.clone());
            }
        }

        if state.remote_repos.task.is_some() {
            ui.spinner();
        }
    });

    ui.add_space(6.0);
    ui.small(labels.connected_repos_hint);
    ui.small(labels.clone_protocol_hint);
    ui.add_space(6.0);

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

fn account_label(account: &ProviderAccount) -> String {
    format!(
        "{} ({})",
        account.display_name,
        provider_host_label(&account.id.kind)
    )
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
        },
    }
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
fn start_clone_with_policy(app: &mut MergeFoxApp, url: String, dest: PathBuf) {
    let policy = app.config.clone_defaults.size_policy;
    let shallow_depth = app.config.clone_defaults.shallow_depth;
    let threshold_mb = app.config.clone_defaults.prompt_threshold_mb;

    let Some(state) = app.active_welcome_state_mut() else {
        return;
    };
    if state.clone.is_some() || state.clone_preflight.is_some() {
        return; // already doing something
    }

    match policy {
        CloneSizePolicy::AlwaysFull => {
            state.clone = Some(clone::spawn(url, dest, None));
        }
        CloneSizePolicy::AlwaysShallow => {
            state.clone = Some(clone::spawn(url, dest, Some(shallow_depth)));
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
            let can_probe = matches!(
                host.as_deref(),
                Some("github.com") | Some("gitlab.com")
            );
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
                state.clone = Some(clone::spawn(url, dest, None));
            }
        }
    }
}

/// Apply the user's choice in the large-repo prompt.
fn apply_clone_decision(app: &mut MergeFoxApp, decision: CloneDecision) {
    let shallow_depth = app.config.clone_defaults.shallow_depth;
    let Some(state) = app.active_welcome_state_mut() else {
        return;
    };
    let Some(prompt) = state.clone_size_prompt.take() else {
        return;
    };
    match decision {
        CloneDecision::Full => {
            state.clone = Some(clone::spawn(prompt.url, prompt.dest, None));
        }
        CloneDecision::Shallow => {
            state.clone = Some(clone::spawn(prompt.url, prompt.dest, Some(shallow_depth)));
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
    let threshold_bytes =
        (app.config.clone_defaults.prompt_threshold_mb as u64) * 1024 * 1024;
    let shallow_depth = app.config.clone_defaults.shallow_depth;
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
            state.clone = Some(clone::spawn(preflight.url, preflight.dest, None));
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
