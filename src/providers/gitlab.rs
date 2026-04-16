//! GitLab.com REST v4.
//!
//! GitLab addresses projects by a single URL-encoded `namespace/project`
//! path segment (e.g. `gitlab-org%2Fgitlab`), not by two owner/repo path
//! components the way GitHub does. That URL-encoding is NOT optional — an
//! unencoded `/` 404s.

use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use super::error::{ProviderError, ProviderResult};
use super::types::{ProviderKind, RepoMeta};

pub struct GitLabProvider;

impl GitLabProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GitLabProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct ProjectResp {
    path: String,
    namespace: NamespaceResp,
    default_branch: Option<String>,
    description: Option<String>,
    visibility: String,
    ssh_url_to_repo: Option<String>,
    http_url_to_repo: Option<String>,
}

#[derive(Deserialize)]
struct NamespaceResp {
    full_path: String,
}

impl super::Provider for GitLabProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::GitLab
    }
    fn display_name(&self) -> &'static str {
        "GitLab"
    }
    fn default_ssh_host(&self) -> &'static str {
        "gitlab.com"
    }
    fn api_base(&self) -> String {
        "https://gitlab.com/api/v4".into()
    }

    fn discover_repo<'a>(
        &'a self,
        client: &'a Client,
        token: Option<&'a SecretString>,
        owner: &'a str,
        repo: &'a str,
    ) -> super::BoxFuture<'a, ProviderResult<RepoMeta>> {
        Box::pin(async move {
            // GitLab projects are indexed by a single URL-encoded segment.
            // We percent-encode the path separator ourselves; the rest of
            // owner/repo are GitLab-valid chars (no spaces, no unicode).
            let ident = format!("{}%2F{}", owner, repo);
            let url = format!("{}/projects/{}", self.api_base(), ident);
            let mut req = client.get(&url);
            if let Some(t) = token {
                // GitLab accepts both `PRIVATE-TOKEN` (for PATs) and
                // `Authorization: Bearer` (for OAuth). PAT form is universal.
                req = req.header("PRIVATE-TOKEN", t.expose_secret());
            }

            let resp = req.send().await?;
            match resp.status().as_u16() {
                200 => {}
                401 => return Err(ProviderError::Unauthorized),
                404 => return Err(ProviderError::NotFound),
                s => {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(ProviderError::Api { status: s, body });
                }
            }

            let p: ProjectResp = resp.json().await?;
            // Prefer GitLab's own clone URLs — they're authoritative for
            // custom ports, self-hosted subpaths, etc.
            let owner_path = p.namespace.full_path;
            let repo_name = p.path;
            let clone_https = p
                .http_url_to_repo
                .unwrap_or_else(|| format!("https://gitlab.com/{owner_path}/{repo_name}.git"));
            let clone_ssh = p
                .ssh_url_to_repo
                .unwrap_or_else(|| format!("git@gitlab.com:{owner_path}/{repo_name}.git"));

            Ok(RepoMeta {
                owner: owner_path,
                repo: repo_name,
                default_branch: p.default_branch.unwrap_or_else(|| "main".into()),
                description: p.description,
                // GitLab exposes three visibilities; anything other than
                // `public` we conservatively treat as private for UI warnings.
                private: p.visibility != "public",
                clone_https,
                clone_ssh,
            })
        })
    }
}
