//! Bitbucket Cloud v2.0 — stub.
//!
//! Bitbucket Cloud's "App Passwords" act as PATs and 2.0 API is
//! `GET /2.0/repositories/{workspace}/{repo_slug}`. Left unimplemented
//! until we have a reviewer with a Bitbucket account to test scopes.

use reqwest::Client;
use secrecy::SecretString;

use super::error::{ProviderError, ProviderResult};
use super::types::{ProviderKind, RepoMeta};

pub struct BitbucketProvider;

impl BitbucketProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for BitbucketProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl super::Provider for BitbucketProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Bitbucket
    }
    fn display_name(&self) -> &'static str {
        "Bitbucket"
    }
    fn default_ssh_host(&self) -> &'static str {
        "bitbucket.org"
    }
    fn api_base(&self) -> String {
        "https://api.bitbucket.org/2.0".into()
    }

    fn discover_repo<'a>(
        &'a self,
        _client: &'a Client,
        _token: Option<&'a SecretString>,
        _owner: &'a str,
        _repo: &'a str,
    ) -> super::BoxFuture<'a, ProviderResult<RepoMeta>> {
        Box::pin(async move {
            Err(ProviderError::NotImplemented(
                "Bitbucket discover_repo is not yet wired up; use Clone-by-URL instead.",
            ))
        })
    }
}
