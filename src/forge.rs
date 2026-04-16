use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::Config;
use crate::git::Repo;
use crate::providers::{
    AccountId, IssueRef, IssueSummary, ProviderKind, PullRequestRef, PullRequestSummary,
};

#[derive(Debug, Clone, Default)]
pub struct ForgeState {
    pub repo: Option<ForgeRepoContext>,
    pub pull_requests: Vec<PullRequestSummary>,
    pub issues: Vec<IssueSummary>,
    pub selected: Option<ForgeSelection>,
    pub pr_modal: Option<PullRequestModalState>,
    pub issue_modal: Option<IssueModalState>,
    pub loaded_once: bool,
    pub loading: bool,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ForgeRepoContext {
    pub account_id: AccountId,
    pub kind: ProviderKind,
    pub owner: String,
    pub repo: String,
    pub remote_name: String,
    pub remote_url: String,
    pub web_url: String,
    pub default_branch: String,
    pub private: bool,
    pub pr_template: Option<String>,
    pub issue_template: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeSelection {
    PullRequest(u64),
    Issue(u64),
}

#[derive(Debug, Clone)]
pub struct PullRequestModalState {
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
    pub draft: bool,
    pub head_ready: bool,
    pub head_hint: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct IssueModalState {
    pub title: String,
    pub body: String,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ForgeRefreshResult {
    pub repo_path: PathBuf,
    pub repo: ForgeRepoContext,
    pub pull_requests: Vec<PullRequestSummary>,
    pub issues: Vec<IssueSummary>,
}

#[derive(Debug, Clone)]
pub struct ForgeCreatePrResult {
    pub repo_path: PathBuf,
    pub pull_request: PullRequestRef,
}

#[derive(Debug, Clone)]
pub struct ForgeCreateIssueResult {
    pub repo_path: PathBuf,
    pub issue: IssueRef,
}

pub fn resolve_repo(config: &Config, repo: &Repo) -> Option<ForgeRepoContext> {
    let preferred_remote = config.repo_settings_for(repo.path()).default_remote;
    let remotes = repo.list_remotes().ok()?;
    let remote = select_remote(&remotes, preferred_remote.as_deref())?;
    let remote_url = remote
        .push_url
        .clone()
        .or(remote.fetch_url.clone())
        .unwrap_or_default();
    let parsed = crate::git_url::parse(&remote_url)?;

    let kind = match parsed.host.as_str() {
        "github.com" => ProviderKind::GitHub,
        "codeberg.org" => ProviderKind::Codeberg,
        "gitlab.com" => ProviderKind::GitLab,
        "bitbucket.org" => ProviderKind::Bitbucket,
        host => ProviderKind::Generic {
            host: host.to_string(),
        },
    };
    if !matches!(kind, ProviderKind::GitHub) {
        return None;
    }
    let account = config
        .provider_accounts
        .iter()
        .find(|account| account.id.kind == kind)
        .map(|account| account.id.clone())?;
    let owner = parsed.owner.clone();
    let repo_name = parsed.repo.clone();
    let web_url = format!("https://{}/{}/{}", parsed.host, owner, repo_name);

    Some(ForgeRepoContext {
        account_id: account,
        kind,
        owner,
        repo: repo_name,
        remote_name: remote.name.clone(),
        remote_url,
        web_url,
        default_branch: "main".to_string(),
        private: false,
        pr_template: load_local_pull_request_template(repo.path()),
        issue_template: load_local_issue_template(repo.path()),
    })
}

pub fn merge_refresh(state: &mut ForgeState, refresh: ForgeRefreshResult) {
    state.repo = Some(refresh.repo);
    state.pull_requests = refresh.pull_requests;
    state.issues = refresh.issues;
    state.loaded_once = true;
    state.loading = false;
    state.last_error = None;
    match state.selected {
        Some(ForgeSelection::PullRequest(number))
            if !state.pull_requests.iter().any(|pr| pr.number == number) =>
        {
            state.selected = None;
        }
        Some(ForgeSelection::Issue(number))
            if !state.issues.iter().any(|issue| issue.number == number) =>
        {
            state.selected = None;
        }
        _ => {}
    }
}

pub fn open_pull_request_modal(
    state: &mut ForgeState,
    head_branch: Option<String>,
    head_ready: bool,
    head_hint: Option<String>,
) {
    let Some(repo) = state.repo.as_ref() else {
        return;
    };
    let head = head_branch.unwrap_or_else(|| repo.default_branch.clone());
    let title = state
        .selected_pull_request()
        .map(|pr| pr.title.clone())
        .unwrap_or_default();
    state.pr_modal = Some(PullRequestModalState {
        title,
        body: repo.pr_template.clone().unwrap_or_default(),
        head,
        base: repo.default_branch.clone(),
        draft: false,
        head_ready,
        head_hint,
        last_error: None,
    });
}

pub fn open_issue_modal(state: &mut ForgeState) {
    let Some(repo) = state.repo.as_ref() else {
        return;
    };
    state.issue_modal = Some(IssueModalState {
        title: String::new(),
        body: repo.issue_template.clone().unwrap_or_default(),
        last_error: None,
    });
}

impl ForgeState {
    pub fn selected_pull_request(&self) -> Option<&PullRequestSummary> {
        let ForgeSelection::PullRequest(number) = self.selected? else {
            return None;
        };
        self.pull_requests.iter().find(|pr| pr.number == number)
    }

    pub fn selected_issue(&self) -> Option<&IssueSummary> {
        let ForgeSelection::Issue(number) = self.selected? else {
            return None;
        };
        self.issues.iter().find(|issue| issue.number == number)
    }
}

pub async fn load_remote_template(
    repo: &ForgeRepoContext,
    path: &str,
) -> crate::providers::ProviderResult<Option<String>> {
    let provider = crate::providers::build(&repo.kind).await;
    let client = crate::providers::default_http_client();
    let token = crate::providers::pat::load_pat(&repo.account_id)
        .map_err(|err| crate::providers::ProviderError::Network(err.to_string()))?
        .ok_or(crate::providers::ProviderError::Unauthorized)?;
    provider
        .load_repo_text_file(&client, &token, &repo.owner, &repo.repo, path)
        .await
}

pub fn candidate_pr_template_paths() -> &'static [&'static str] {
    &[
        ".github/PULL_REQUEST_TEMPLATE.md",
        ".github/pull_request_template.md",
        "PULL_REQUEST_TEMPLATE.md",
        "docs/PULL_REQUEST_TEMPLATE.md",
    ]
}

pub fn candidate_issue_template_paths() -> &'static [&'static str] {
    &[
        ".github/ISSUE_TEMPLATE/bug_report.md",
        ".github/ISSUE_TEMPLATE/issue.md",
        ".github/ISSUE_TEMPLATE.md",
        "docs/ISSUE_TEMPLATE.md",
    ]
}

fn load_local_pull_request_template(repo_path: &Path) -> Option<String> {
    load_first_existing(repo_path, candidate_pr_template_paths())
}

fn load_local_issue_template(repo_path: &Path) -> Option<String> {
    load_first_existing(repo_path, candidate_issue_template_paths())
}

fn load_first_existing(repo_path: &Path, candidates: &[&str]) -> Option<String> {
    for relative in candidates {
        let full = repo_path.join(relative);
        if let Ok(text) = fs::read_to_string(&full) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn select_remote<'a>(
    remotes: &'a [crate::git::RemoteInfo],
    preferred: Option<&str>,
) -> Option<&'a crate::git::RemoteInfo> {
    preferred
        .and_then(|name| remotes.iter().find(|remote| remote.name == name))
        .or_else(|| remotes.iter().find(|remote| remote.name == "origin"))
        .or_else(|| remotes.first())
}

pub fn pull_request_scope_text(private: bool) -> &'static str {
    if private {
        "repo"
    } else {
        "public_repo"
    }
}

pub fn issue_scope_text() -> &'static str {
    "repo"
}

pub fn is_github_repo(repo: &ForgeRepoContext) -> bool {
    matches!(repo.kind, ProviderKind::GitHub)
}

pub fn _ensure_result(_: Result<()>) {}
