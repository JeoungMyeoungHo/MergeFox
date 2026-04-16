//! Remote repository provider abstraction.
//!
//! One trait (`Provider`) fronts every supported git host: GitHub, GitLab,
//! Bitbucket, Azure DevOps, Codeberg, Gitea (self-hosted), and a catch-all
//! "Generic" host. Each concrete impl owns its REST quirks (auth header
//! name, repo-lookup URL shape, JSON field names) so the rest of the app
//! stays provider-agnostic.
//!
//! Why a single trait instead of one function per provider?
//! - The UI wants to iterate over configured accounts uniformly.
//! - Future features (list-branches, create-PR) will add methods to the same
//!   trait — concrete providers opt out with `NotImplemented` rather than
//!   forcing every call site to know the set of providers.
//!
//! Why `BoxFuture` instead of native async-in-trait?
//! - Native `async fn` in traits (stable since 1.75) makes the trait not
//!   dyn-safe. The UI stores providers as `Box<dyn Provider>` so we need
//!   dyn compatibility. Returning `Pin<Box<dyn Future>>` is the standard
//!   workaround that also avoids pulling in `async-trait` as a new dep.
//!
//! Secret hygiene: any module that touches tokens (`oauth`, `pat`, `ssh`)
//! deals in `SecretString` only. Tokens never reach `config.json` — the
//! keyring is the single source of truth.

use std::future::Future;
use std::pin::Pin;

use reqwest::Client;
use secrecy::SecretString;

pub mod error;
pub mod oauth;
pub mod pat;
pub mod runtime;
pub mod ssh;
pub mod types;

mod azure;
mod bitbucket;
mod generic;
mod gitea;
mod github;
mod gitlab;

pub use azure::AzureDevOpsProvider;
pub use bitbucket::BitbucketProvider;
pub use error::{ProviderError, ProviderResult};
pub use generic::GenericProvider;
pub use gitea::GiteaProvider;
pub use github::GitHubProvider;
pub use gitlab::GitLabProvider;
pub use types::*;

/// Manual lowering of what `#[async_trait]` would generate — a pinned,
/// boxed future tied to the `'a` borrow of the trait method's arguments.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One remote host. Trait methods take a shared `reqwest::Client` so
/// connection pooling is reused across calls; the caller is responsible
/// for building a client with whatever timeouts / proxies they want.
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn display_name(&self) -> &'static str;
    /// Host portion of the canonical SSH URL (`git@<host>:owner/repo`).
    /// Generic / self-hosted providers may return `""` — callers should
    /// check `kind()` and fall back to the carried instance URL.
    fn default_ssh_host(&self) -> &'static str;
    /// REST API base without trailing slash.
    fn api_base(&self) -> String;

    /// Fetch repo metadata. `token` is optional — public repos on public
    /// providers can be queried anonymously (subject to rate limits).
    fn discover_repo<'a>(
        &'a self,
        client: &'a Client,
        token: Option<&'a SecretString>,
        owner: &'a str,
        repo: &'a str,
    ) -> BoxFuture<'a, ProviderResult<RepoMeta>>;

    /// Fetch the currently-authenticated user/account profile.
    ///
    /// Default: provider does not expose or we haven't implemented it yet.
    fn current_user<'a>(
        &'a self,
        _client: &'a Client,
        _token: &'a SecretString,
    ) -> BoxFuture<'a, ProviderResult<ProviderProfile>> {
        Box::pin(async move { Err(ProviderError::NotImplemented("current_user")) })
    }

    /// List repositories the authenticated account can see on this host.
    ///
    /// Intended for launcher/home "clone from connected account" flows.
    fn list_accessible_repositories<'a>(
        &'a self,
        _client: &'a Client,
        _token: &'a SecretString,
    ) -> BoxFuture<'a, ProviderResult<Vec<RemoteRepoSummary>>> {
        Box::pin(async move {
            Err(ProviderError::NotImplemented(
                "list_accessible_repositories",
            ))
        })
    }

    fn create_pull_request<'a>(
        &'a self,
        _client: &'a Client,
        _token: &'a SecretString,
        _req: &'a PullRequestDraft,
    ) -> BoxFuture<'a, ProviderResult<PullRequestRef>> {
        Box::pin(async move { Err(ProviderError::NotImplemented("create_pull_request")) })
    }

    fn list_pull_requests<'a>(
        &'a self,
        _client: &'a Client,
        _token: &'a SecretString,
        _owner: &'a str,
        _repo: &'a str,
        _state: PrState,
    ) -> BoxFuture<'a, ProviderResult<Vec<PullRequestSummary>>> {
        Box::pin(async move { Err(ProviderError::NotImplemented("list_pull_requests")) })
    }

    fn create_issue<'a>(
        &'a self,
        _client: &'a Client,
        _token: &'a SecretString,
        _req: &'a IssueDraft,
    ) -> BoxFuture<'a, ProviderResult<IssueRef>> {
        Box::pin(async move { Err(ProviderError::NotImplemented("create_issue")) })
    }

    fn list_issues<'a>(
        &'a self,
        _client: &'a Client,
        _token: &'a SecretString,
        _owner: &'a str,
        _repo: &'a str,
        _state: IssueState,
    ) -> BoxFuture<'a, ProviderResult<Vec<IssueSummary>>> {
        Box::pin(async move { Err(ProviderError::NotImplemented("list_issues")) })
    }

    fn load_repo_text_file<'a>(
        &'a self,
        _client: &'a Client,
        _token: &'a SecretString,
        _owner: &'a str,
        _repo: &'a str,
        _path: &'a str,
    ) -> BoxFuture<'a, ProviderResult<Option<String>>> {
        Box::pin(async move { Err(ProviderError::NotImplemented("load_repo_text_file")) })
    }
}

/// Build a boxed provider from a `ProviderKind`. Async only to leave room
/// for future network-dependent construction (e.g. probing a Gitea
/// instance's capabilities); today every branch is synchronous.
pub async fn build(kind: &ProviderKind) -> Box<dyn Provider> {
    match kind {
        ProviderKind::GitHub => Box::new(GitHubProvider::new()),
        ProviderKind::GitLab => Box::new(GitLabProvider::new()),
        ProviderKind::Bitbucket => Box::new(BitbucketProvider::new()),
        ProviderKind::AzureDevOps => Box::new(AzureDevOpsProvider::new()),
        ProviderKind::Codeberg => Box::new(GiteaProvider::codeberg()),
        ProviderKind::Gitea { instance } => Box::new(GiteaProvider::new(instance.clone())),
        ProviderKind::Generic { host } => Box::new(GenericProvider::new(host.clone())),
    }
}

/// Build a `reqwest::Client` with sensible defaults for provider REST work.
///
/// Kept here rather than in each concrete impl so timeout/UA changes
/// propagate everywhere at once.
pub fn default_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        // GitHub rejects requests without a UA; harmless for other providers.
        .user_agent("mergefox/0.1")
        .build()
        // `reqwest::Client::builder().build()` only fails if the TLS
        // backend can't initialize — at that point nothing else will work
        // either, so panicking early is the honest thing to do.
        .expect("build reqwest client")
}
