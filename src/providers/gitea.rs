//! Gitea / Forgejo (self-hosted) — stub.
//!
//! Gitea's API is near-GitHub-compatible: `GET /api/v1/repos/{owner}/{repo}`
//! returns fields with the same names as GitHub (default_branch, private, …).
//! The only unknown is the instance URL, which `ProviderKind::Gitea` carries.
//!
//! Leaving the HTTP call itself stubbed so the first real Gitea user can
//! confirm their instance's TLS/self-signed-cert posture rather than us
//! guessing. Codeberg reuses this provider with `instance = https://codeberg.org`.

use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use super::error::{ProviderError, ProviderResult};
use super::types::{ProviderKind, ProviderProfile, RepoMeta};

pub struct GiteaProvider {
    /// Instance base URL, e.g. `https://codeberg.org`. Never trailing-slashed.
    pub instance: String,
    /// If true, advertise via `ProviderKind::Codeberg` instead of `Gitea` —
    /// purely cosmetic so the UI can show a Codeberg-specific label/icon.
    pub as_codeberg: bool,
}

#[derive(Deserialize)]
struct UserResp {
    login: String,
    #[serde(default)]
    full_name: Option<String>,
    #[serde(default)]
    avatar_url: Option<String>,
}

impl GiteaProvider {
    pub fn new(instance: impl Into<String>) -> Self {
        Self {
            instance: instance.into().trim_end_matches('/').to_string(),
            as_codeberg: false,
        }
    }

    pub fn codeberg() -> Self {
        Self {
            instance: "https://codeberg.org".into(),
            as_codeberg: true,
        }
    }
}

impl super::Provider for GiteaProvider {
    fn kind(&self) -> ProviderKind {
        if self.as_codeberg {
            ProviderKind::Codeberg
        } else {
            ProviderKind::Gitea {
                instance: self.instance.clone(),
            }
        }
    }

    fn display_name(&self) -> &'static str {
        if self.as_codeberg {
            "Codeberg"
        } else {
            "Gitea"
        }
    }

    fn default_ssh_host(&self) -> &'static str {
        // Best-effort constant — for custom Gitea, callers should parse
        // `instance` themselves. Codeberg is the common case.
        if self.as_codeberg {
            "codeberg.org"
        } else {
            "" // unknown; UI should surface the instance URL instead.
        }
    }

    fn api_base(&self) -> String {
        format!("{}/api/v1", self.instance)
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
                "Gitea discover_repo stub — needs per-instance TLS testing before enabling.",
            ))
        })
    }

    fn current_user<'a>(
        &'a self,
        client: &'a Client,
        token: &'a SecretString,
    ) -> super::BoxFuture<'a, ProviderResult<ProviderProfile>> {
        Box::pin(async move {
            let resp = client
                .get(format!("{}/user", self.api_base()))
                .header("Accept", "application/json")
                .header("Authorization", format!("token {}", token.expose_secret()))
                .send()
                .await?;

            match resp.status().as_u16() {
                200 => {}
                401 | 403 => return Err(ProviderError::Unauthorized),
                404 => return Err(ProviderError::NotFound),
                s => {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(ProviderError::Api { status: s, body });
                }
            }

            let user: UserResp = resp.json().await?;
            Ok(ProviderProfile {
                username: user.login.clone(),
                display_name: user.full_name.unwrap_or(user.login),
                avatar_url: user.avatar_url,
            })
        })
    }
}
