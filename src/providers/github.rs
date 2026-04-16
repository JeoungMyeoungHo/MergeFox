//! GitHub.com REST v3 implementation.
//!
//! We deliberately use the classic REST endpoint rather than GraphQL —
//! a single `GET /repos/{o}/{r}` is enough for pre-clone metadata and
//! avoids the weight (+ 10-req/min anonymous ceiling) of GraphQL.

// NOTE: `async_trait` is intentionally NOT in Cargo.toml. The trait returns
// a `BoxFuture` (defined in mod.rs) — this is the manual lowering of what
// `#[async_trait]` would generate, without the proc-macro dep.

use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use super::error::{ProviderError, ProviderResult};
use super::types::{
    CreateRepositoryDraft, CreatedRepositoryRef, IssueDraft, IssueRef, IssueState, IssueSummary,
    PrState, ProviderKind, ProviderProfile, PullRequestDraft, PullRequestRef,
    PullRequestSummary, RemoteRepoOwner, RemoteRepoOwnerKind, RemoteRepoSummary, RepoMeta,
};

pub struct GitHubProvider;

impl GitHubProvider {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_repository_url_uses_user_endpoint() {
        let url = create_repository_url(
            "https://api.github.com",
            &CreateRepositoryDraft {
                owner: "alice".into(),
                owner_kind: RemoteRepoOwnerKind::User,
                name: "demo".into(),
                description: None,
                private: true,
                auto_init: false,
            },
        );

        assert_eq!(url, "https://api.github.com/user/repos");
    }

    #[test]
    fn create_repository_url_uses_org_endpoint() {
        let url = create_repository_url(
            "https://api.github.com",
            &CreateRepositoryDraft {
                owner: "acme".into(),
                owner_kind: RemoteRepoOwnerKind::Organization,
                name: "demo".into(),
                description: None,
                private: true,
                auto_init: false,
            },
        );

        assert_eq!(url, "https://api.github.com/orgs/acme/repos");
    }
}

impl Default for GitHubProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct RepoResp {
    name: String,
    owner: OwnerResp,
    default_branch: Option<String>,
    description: Option<String>,
    private: bool,
}

#[derive(Deserialize)]
struct RepoListResp {
    name: String,
    owner: OwnerResp,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    description: Option<String>,
    private: bool,
    ssh_url: String,
    clone_url: String,
    html_url: String,
}

#[derive(Deserialize)]
struct OwnerResp {
    login: String,
}

#[derive(Deserialize)]
struct UserResp {
    login: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct OrgResp {
    login: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct PullRequestResp {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    draft: bool,
    state: String,
    user: OwnerResp,
    head: GitRefResp,
    base: GitRefResp,
}

#[derive(Deserialize)]
struct GitRefResp {
    #[serde(rename = "ref")]
    name: String,
}

#[derive(Deserialize)]
struct IssueResp {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    state: String,
    user: OwnerResp,
    #[serde(default)]
    labels: Vec<LabelResp>,
    #[serde(default)]
    assignees: Vec<OwnerResp>,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct LabelResp {
    name: String,
}

#[derive(serde::Serialize)]
struct CreatePrBody<'a> {
    title: &'a str,
    body: &'a str,
    head: &'a str,
    base: &'a str,
    draft: bool,
}

#[derive(serde::Serialize)]
struct CreateIssueBody<'a> {
    title: &'a str,
    body: &'a str,
}

#[derive(serde::Serialize)]
struct CreateRepositoryBody<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    private: bool,
    auto_init: bool,
}

#[derive(Deserialize)]
struct CreateRepositoryResp {
    name: String,
    owner: OwnerResp,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    description: Option<String>,
    private: bool,
    ssh_url: String,
    clone_url: String,
    html_url: String,
}

fn apply_headers(req: reqwest::RequestBuilder, token: &SecretString) -> reqwest::RequestBuilder {
    req.header("User-Agent", "mergefox")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("token {}", token.expose_secret()))
}

fn pr_state_from_api(state: &str) -> PrState {
    match state {
        "closed" => PrState::Closed,
        _ => PrState::Open,
    }
}

fn issue_state_from_api(state: &str) -> IssueState {
    match state {
        "closed" => IssueState::Closed,
        _ => IssueState::Open,
    }
}

fn owner_display_name(login: &str, name: Option<String>) -> String {
    name.filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| login.to_string())
}

fn create_repository_url(api_base: &str, req: &CreateRepositoryDraft) -> String {
    match req.owner_kind {
        RemoteRepoOwnerKind::User => format!("{api_base}/user/repos"),
        RemoteRepoOwnerKind::Organization => format!("{api_base}/orgs/{}/repos", req.owner),
    }
}

async fn map_status(resp: reqwest::Response) -> ProviderResult<reqwest::Response> {
    match resp.status().as_u16() {
        200 | 201 => Ok(resp),
        401 => Err(ProviderError::Unauthorized),
        403 => {
            let rate_remaining = resp
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            if rate_remaining == Some(0) {
                Err(ProviderError::RateLimited { retry_after: None })
            } else {
                Err(ProviderError::Unauthorized)
            }
        }
        404 => Err(ProviderError::NotFound),
        status => {
            let body = resp.text().await.unwrap_or_default();
            Err(ProviderError::Api { status, body })
        }
    }
}

impl super::Provider for GitHubProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::GitHub
    }

    fn display_name(&self) -> &'static str {
        "GitHub"
    }

    fn default_ssh_host(&self) -> &'static str {
        "github.com"
    }

    fn api_base(&self) -> String {
        "https://api.github.com".into()
    }

    fn discover_repo<'a>(
        &'a self,
        client: &'a Client,
        token: Option<&'a SecretString>,
        owner: &'a str,
        repo: &'a str,
    ) -> super::BoxFuture<'a, ProviderResult<RepoMeta>> {
        Box::pin(async move {
            let url = format!("{}/repos/{}/{}", self.api_base(), owner, repo);
            let mut req = client
                .get(&url)
                // GitHub requires a User-Agent header on every request.
                .header("User-Agent", "mergefox")
                // Pin API version so undocumented field changes don't silently
                // break our parser.
                .header("X-GitHub-Api-Version", "2022-11-28")
                .header("Accept", "application/vnd.github+json");

            if let Some(t) = token {
                // `token` prefix is the classic-PAT form; it also works with
                // fine-grained PATs and user-to-server OAuth tokens.
                req = req.header("Authorization", format!("token {}", t.expose_secret()));
            }

            let resp = req.send().await?;
            let resp = map_status(resp).await?;
            let r: RepoResp = resp.json().await?;
            let owner = r.owner.login;
            let repo_name = r.name;
            Ok(RepoMeta {
                clone_https: format!("https://github.com/{owner}/{repo_name}.git"),
                clone_ssh: format!("git@github.com:{owner}/{repo_name}.git"),
                // GitHub always exposes a default branch for non-empty repos;
                // empty repos may omit it — fall back to the GitHub default.
                default_branch: r.default_branch.unwrap_or_else(|| "main".into()),
                description: r.description,
                private: r.private,
                owner,
                repo: repo_name,
            })
        })
    }

    fn current_user<'a>(
        &'a self,
        client: &'a Client,
        token: &'a SecretString,
    ) -> super::BoxFuture<'a, ProviderResult<ProviderProfile>> {
        Box::pin(async move {
            let resp = apply_headers(client.get(format!("{}/user", self.api_base())), token)
                .send()
                .await?;
            let resp = map_status(resp).await?;

            let user: UserResp = resp.json().await?;
            Ok(ProviderProfile {
                username: user.login.clone(),
                display_name: user.name.unwrap_or(user.login),
                avatar_url: user.avatar_url,
            })
        })
    }

    fn list_accessible_repositories<'a>(
        &'a self,
        client: &'a Client,
        token: &'a SecretString,
    ) -> super::BoxFuture<'a, ProviderResult<Vec<RemoteRepoSummary>>> {
        Box::pin(async move {
            let mut repos = Vec::new();

            for page in 1..=10 {
                let page = page.to_string();
                let resp =
                    apply_headers(client.get(format!("{}/user/repos", self.api_base())), token)
                        .query(&[
                            ("visibility", "all"),
                            ("affiliation", "owner,collaborator,organization_member"),
                            ("sort", "updated"),
                            ("per_page", "100"),
                            ("page", page.as_str()),
                        ])
                        .send()
                        .await?;
                let resp = map_status(resp).await?;
                let batch: Vec<RepoListResp> = resp.json().await?;
                let count = batch.len();

                repos.extend(batch.into_iter().map(|repo| RemoteRepoSummary {
                    owner: repo.owner.login,
                    repo: repo.name,
                    description: repo.description,
                    default_branch: repo.default_branch,
                    private: repo.private,
                    clone_https: repo.clone_url,
                    clone_ssh: repo.ssh_url,
                    web_url: repo.html_url,
                }));

                if count < 100 {
                    break;
                }
            }

            Ok(repos)
        })
    }

    fn list_repository_owners<'a>(
        &'a self,
        client: &'a Client,
        token: &'a SecretString,
    ) -> super::BoxFuture<'a, ProviderResult<Vec<RemoteRepoOwner>>> {
        Box::pin(async move {
            let resp = apply_headers(client.get(format!("{}/user", self.api_base())), token)
                .send()
                .await?;
            let resp = map_status(resp).await?;
            let user: UserResp = resp.json().await?;

            let mut owners = vec![RemoteRepoOwner {
                login: user.login.clone(),
                display_name: owner_display_name(&user.login, user.name),
                kind: RemoteRepoOwnerKind::User,
            }];

            for page in 1..=10 {
                let page = page.to_string();
                let resp =
                    apply_headers(client.get(format!("{}/user/orgs", self.api_base())), token)
                        .query(&[("per_page", "100"), ("page", page.as_str())])
                        .send()
                        .await?;
                let resp = map_status(resp).await?;
                let batch: Vec<OrgResp> = resp.json().await?;
                let count = batch.len();

                owners.extend(batch.into_iter().map(|org| RemoteRepoOwner {
                    login: org.login.clone(),
                    display_name: owner_display_name(&org.login, org.name),
                    kind: RemoteRepoOwnerKind::Organization,
                }));

                if count < 100 {
                    break;
                }
            }

            Ok(owners)
        })
    }

    fn create_repository<'a>(
        &'a self,
        client: &'a Client,
        token: &'a SecretString,
        req: &'a CreateRepositoryDraft,
    ) -> super::BoxFuture<'a, ProviderResult<CreatedRepositoryRef>> {
        Box::pin(async move {
            let description = req
                .description
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let resp = apply_headers(
                client.post(create_repository_url(&self.api_base(), req)),
                token,
            )
            .json(&CreateRepositoryBody {
                name: &req.name,
                description,
                private: req.private,
                auto_init: req.auto_init,
            })
            .send()
            .await?;
            let resp = map_status(resp).await?;
            let repo: CreateRepositoryResp = resp.json().await?;
            Ok(CreatedRepositoryRef {
                owner: repo.owner.login,
                repo: repo.name,
                description: repo.description,
                default_branch: repo.default_branch,
                private: repo.private,
                clone_https: repo.clone_url,
                clone_ssh: repo.ssh_url,
                web_url: repo.html_url,
            })
        })
    }

    fn create_pull_request<'a>(
        &'a self,
        client: &'a Client,
        token: &'a SecretString,
        req: &'a PullRequestDraft,
    ) -> super::BoxFuture<'a, ProviderResult<PullRequestRef>> {
        Box::pin(async move {
            let url = format!("{}/repos/{}/{}/pulls", self.api_base(), req.owner, req.repo);
            let resp = apply_headers(client.post(url), token)
                .json(&CreatePrBody {
                    title: &req.title,
                    body: &req.body,
                    head: &req.head,
                    base: &req.base,
                    draft: req.draft,
                })
                .send()
                .await?;
            let resp = map_status(resp).await?;
            let pr: PullRequestResp = resp.json().await?;
            Ok(PullRequestRef {
                number: pr.number,
                title: pr.title,
                url: pr.html_url,
            })
        })
    }

    fn list_pull_requests<'a>(
        &'a self,
        client: &'a Client,
        token: &'a SecretString,
        owner: &'a str,
        repo: &'a str,
        state: PrState,
    ) -> super::BoxFuture<'a, ProviderResult<Vec<PullRequestSummary>>> {
        Box::pin(async move {
            let url = format!("{}/repos/{owner}/{repo}/pulls", self.api_base());
            let resp = apply_headers(client.get(url), token)
                .query(&[("state", state.as_api_str()), ("per_page", "30")])
                .send()
                .await?;
            let resp = map_status(resp).await?;
            let prs: Vec<PullRequestResp> = resp.json().await?;
            Ok(prs
                .into_iter()
                .map(|pr| PullRequestSummary {
                    number: pr.number,
                    title: pr.title,
                    body: pr.body,
                    author: pr.user.login,
                    url: pr.html_url,
                    is_draft: pr.draft,
                    state: pr_state_from_api(&pr.state),
                    head_ref: pr.head.name,
                    base_ref: pr.base.name,
                })
                .collect())
        })
    }

    fn create_issue<'a>(
        &'a self,
        client: &'a Client,
        token: &'a SecretString,
        req: &'a IssueDraft,
    ) -> super::BoxFuture<'a, ProviderResult<IssueRef>> {
        Box::pin(async move {
            let url = format!(
                "{}/repos/{}/{}/issues",
                self.api_base(),
                req.owner,
                req.repo
            );
            let resp = apply_headers(client.post(url), token)
                .json(&CreateIssueBody {
                    title: &req.title,
                    body: &req.body,
                })
                .send()
                .await?;
            let resp = map_status(resp).await?;
            let issue: IssueResp = resp.json().await?;
            Ok(IssueRef {
                number: issue.number,
                title: issue.title,
                url: issue.html_url,
            })
        })
    }

    fn list_issues<'a>(
        &'a self,
        client: &'a Client,
        token: &'a SecretString,
        owner: &'a str,
        repo: &'a str,
        state: IssueState,
    ) -> super::BoxFuture<'a, ProviderResult<Vec<IssueSummary>>> {
        Box::pin(async move {
            let url = format!("{}/repos/{owner}/{repo}/issues", self.api_base());
            let resp = apply_headers(client.get(url), token)
                .query(&[("state", state.as_api_str()), ("per_page", "30")])
                .send()
                .await?;
            let resp = map_status(resp).await?;
            let issues: Vec<IssueResp> = resp.json().await?;
            Ok(issues
                .into_iter()
                .filter(|issue| issue.pull_request.is_none())
                .map(|issue| IssueSummary {
                    number: issue.number,
                    title: issue.title,
                    body: issue.body,
                    author: issue.user.login,
                    url: issue.html_url,
                    state: issue_state_from_api(&issue.state),
                    labels: issue.labels.into_iter().map(|label| label.name).collect(),
                    assignees: issue.assignees.into_iter().map(|user| user.login).collect(),
                })
                .collect())
        })
    }

    fn load_repo_text_file<'a>(
        &'a self,
        client: &'a Client,
        token: &'a SecretString,
        owner: &'a str,
        repo: &'a str,
        path: &'a str,
    ) -> super::BoxFuture<'a, ProviderResult<Option<String>>> {
        Box::pin(async move {
            let url = format!("{}/repos/{owner}/{repo}/contents/{path}", self.api_base());
            let resp = apply_headers(
                client
                    .get(url)
                    .header("Accept", "application/vnd.github.raw+json"),
                token,
            )
            .send()
            .await?;
            match resp.status().as_u16() {
                200 => Ok(Some(resp.text().await.unwrap_or_default())),
                404 => Ok(None),
                401 | 403 => Err(ProviderError::Unauthorized),
                status => {
                    let body = resp.text().await.unwrap_or_default();
                    Err(ProviderError::Api { status, body })
                }
            }
        })
    }
}
