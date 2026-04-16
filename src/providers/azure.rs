//! Azure DevOps Services REST — stub.
//!
//! API shape: `GET https://dev.azure.com/{org}/{project}/_apis/git/repositories/{repo}?api-version=7.1`
//! Azure's three-tier org/project/repo hierarchy means `owner/repo` alone
//! is ambiguous — we'll need a separate UI field for `project`. Leaving
//! this stubbed until we design that UI.

use reqwest::Client;
use secrecy::SecretString;

use super::error::{ProviderError, ProviderResult};
use super::types::{ProviderKind, RepoMeta};

pub struct AzureDevOpsProvider;

impl AzureDevOpsProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AzureDevOpsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl super::Provider for AzureDevOpsProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::AzureDevOps
    }
    fn display_name(&self) -> &'static str {
        "Azure DevOps"
    }
    fn default_ssh_host(&self) -> &'static str {
        "ssh.dev.azure.com"
    }
    fn api_base(&self) -> String {
        "https://dev.azure.com".into()
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
                "Azure DevOps requires org/project/repo — three-tier UI not yet built.",
            ))
        })
    }
}
