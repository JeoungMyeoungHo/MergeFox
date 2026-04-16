//! "Generic" provider — any git host without a known REST API.
//!
//! We can still clone (git itself doesn't need the REST API) and store a
//! PAT for HTTPS transport; we just can't fetch repo metadata ahead of time.
//! `discover_repo` therefore fabricates a minimal `RepoMeta` from the inputs
//! so the UI can proceed with a `default_branch` guess.

use reqwest::Client;
use secrecy::SecretString;

use super::error::ProviderResult;
use super::types::{ProviderKind, RepoMeta};

pub struct GenericProvider {
    pub host: String,
}

impl GenericProvider {
    pub fn new(host: impl Into<String>) -> Self {
        Self { host: host.into() }
    }
}

impl super::Provider for GenericProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Generic {
            host: self.host.clone(),
        }
    }
    fn display_name(&self) -> &'static str {
        "Generic"
    }
    fn default_ssh_host(&self) -> &'static str {
        // We can't return a borrowed `self.host` as `&'static str` without
        // leaking — UI should read `ProviderKind::Generic { host }` instead.
        ""
    }
    fn api_base(&self) -> String {
        format!("https://{}", self.host)
    }

    fn discover_repo<'a>(
        &'a self,
        _client: &'a Client,
        _token: Option<&'a SecretString>,
        owner: &'a str,
        repo: &'a str,
    ) -> super::BoxFuture<'a, ProviderResult<RepoMeta>> {
        let host = self.host.clone();
        let owner = owner.to_string();
        let repo = repo.to_string();
        Box::pin(async move {
            // No API, so we fabricate best-effort metadata. `main` is the
            // modern default; if the remote uses something else, git will
            // surface it after clone.
            Ok(RepoMeta {
                clone_https: format!("https://{host}/{owner}/{repo}.git"),
                clone_ssh: format!("git@{host}:{owner}/{repo}.git"),
                default_branch: "main".into(),
                description: None,
                private: false,
                owner,
                repo,
            })
        })
    }
}
