use serde::{Deserialize, Serialize};

use crate::forge::{ForgeSelection, ForgeState};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgeView {
    pub repo: Option<ForgeRepoView>,
    pub loading: bool,
    pub loaded_once: bool,
    pub last_error: Option<String>,
    pub selected: Option<ForgeSelectedView>,
    pub pull_requests: Vec<ForgePullRequestView>,
    pub issues: Vec<ForgeIssueView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgeRepoView {
    pub provider: String,
    pub owner: String,
    pub repo: String,
    pub remote_name: String,
    pub remote_url: String,
    pub web_url: String,
    pub default_branch: String,
    pub private: bool,
    pub pr_template_available: bool,
    pub issue_template_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ForgeSelectedView {
    PullRequest { number: u64 },
    Issue { number: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgePullRequestView {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub author: String,
    pub url: String,
    pub state: String,
    pub draft: bool,
    pub head_ref: String,
    pub base_ref: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgeIssueView {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub author: String,
    pub url: String,
    pub state: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
}

pub fn forge_view_for_state(state: &ForgeState) -> ForgeView {
    ForgeView {
        repo: state.repo.as_ref().map(|repo| ForgeRepoView {
            provider: repo.kind.to_string(),
            owner: repo.owner.clone(),
            repo: repo.repo.clone(),
            remote_name: repo.remote_name.clone(),
            remote_url: repo.remote_url.clone(),
            web_url: repo.web_url.clone(),
            default_branch: repo.default_branch.clone(),
            private: repo.private,
            pr_template_available: repo
                .pr_template
                .as_deref()
                .is_some_and(|t| !t.trim().is_empty()),
            issue_template_available: repo
                .issue_template
                .as_deref()
                .is_some_and(|t| !t.trim().is_empty()),
        }),
        loading: state.loading,
        loaded_once: state.loaded_once,
        last_error: state.last_error.clone(),
        selected: state.selected.map(|selected| match selected {
            ForgeSelection::PullRequest(number) => ForgeSelectedView::PullRequest { number },
            ForgeSelection::Issue(number) => ForgeSelectedView::Issue { number },
        }),
        pull_requests: state
            .pull_requests
            .iter()
            .map(|pr| ForgePullRequestView {
                number: pr.number,
                title: pr.title.clone(),
                body: pr.body.clone(),
                author: pr.author.clone(),
                url: pr.url.clone(),
                state: pr.state.as_api_str().to_string(),
                draft: pr.is_draft,
                head_ref: pr.head_ref.clone(),
                base_ref: pr.base_ref.clone(),
            })
            .collect(),
        issues: state
            .issues
            .iter()
            .map(|issue| ForgeIssueView {
                number: issue.number,
                title: issue.title.clone(),
                body: issue.body.clone(),
                author: issue.author.clone(),
                url: issue.url.clone(),
                state: issue.state.as_api_str().to_string(),
                labels: issue.labels.clone(),
                assignees: issue.assignees.clone(),
            })
            .collect(),
    }
}
