//! OAuth 2.0 Device Authorization Grant (RFC 8628).
//!
//! Why device flow: mergeFox is a desktop GUI app; we can't keep a client
//! secret, and a browser-redirect loopback flow is fragile (antivirus
//! software blocks ephemeral 127.0.0.1 servers, some firewalls too).
//! Device flow puts the auth in the user's browser and polls for a token.
//!
//! GitHub supports device flow for OAuth Apps with the `Iv1.` public
//! client_id below — it's a real registered app and the client_id is
//! documented as public (it can't sign anything on its own).
//!
//! GitLab.com: device flow requires an app admin to register a confidential
//! client. We don't ship one, so GitLab.com is PAT-only today. Self-hosted
//! Gitea supports device flow as of 1.21 but with per-instance client_ids,
//! so we only wire it up from `OAuthDeviceConfig` rather than hardcoding.

use std::time::Duration;

use reqwest::Client;
use secrecy::SecretString;
use serde::Deserialize;

use super::error::{ProviderError, ProviderResult};
use super::types::OAuthDeviceConfig;

/// RFC 8628 §3.2 device authorization response.
///
/// `interval` defaults to 5s per the RFC if the server omits it.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    /// Some providers (GitHub) return this as `verification_uri_complete`;
    /// we don't require it — UI falls back to `verification_uri` + user_code.
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    #[serde(default = "default_interval")]
    pub interval: u64,
}

fn default_interval() -> u64 {
    5
}

/// Successful token response — secret material only leaves via `SecretString`.
#[derive(Debug)]
pub struct TokenResponse {
    pub access_token: SecretString,
    pub token_type: String,
    pub scope: Option<String>,
    pub refresh_token: Option<SecretString>,
    pub expires_in: Option<u64>,
}

/// Raw wire form — we swap to `SecretString` immediately after deserialize.
#[derive(Deserialize)]
struct RawToken {
    access_token: String,
    token_type: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// RFC 8628 §3.5 error body.
#[derive(Deserialize)]
struct TokenErrorBody {
    error: String,
    #[serde(default)]
    #[allow(dead_code)]
    error_description: Option<String>,
}

/// Kick off a device flow — user will be shown `user_code` + `verification_uri`.
pub async fn start_device_flow(
    cfg: &OAuthDeviceConfig,
    http: &Client,
) -> ProviderResult<DeviceCodeResponse> {
    let resp = http
        .post(&cfg.device_auth_url)
        // GitHub wants Accept: application/json or it replies with form-encoded.
        .header("Accept", "application/json")
        .form(&[
            ("client_id", cfg.client_id.as_str()),
            ("scope", cfg.scope.as_str()),
        ])
        .send()
        .await?;

    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(ProviderError::Api {
            status: status.as_u16(),
            body: text,
        });
    }

    serde_json::from_str(&text).map_err(ProviderError::from)
}

/// Poll the token endpoint until the user approves or we hit a terminal error.
///
/// Implements `slow_down` — the authorization server can ask us to back off
/// by 5s (per RFC). We also respect `interval` bumps across successive polls.
pub async fn poll_token(
    cfg: &OAuthDeviceConfig,
    device_code: &str,
    initial_interval: Duration,
    http: &Client,
) -> ProviderResult<TokenResponse> {
    let mut interval = initial_interval;
    // Hard cap so a broken server can't spin us forever. 15 minutes is
    // longer than any real device-flow expires_in we've seen in the wild.
    let max_total = Duration::from_secs(15 * 60);
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > max_total {
            return Err(ProviderError::Api {
                status: 408,
                body: "device flow polling exceeded local timeout".into(),
            });
        }

        tokio::time::sleep(interval).await;

        let resp = http
            .post(&cfg.token_url)
            .header("Accept", "application/json")
            .form(&[
                ("client_id", cfg.client_id.as_str()),
                ("device_code", device_code),
                // RFC-mandated grant type for device flow.
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await?;

        // Success: status 200 AND body is a token (not an error).
        if status.is_success() {
            if let Ok(raw) = serde_json::from_str::<RawToken>(&text) {
                return Ok(TokenResponse {
                    access_token: SecretString::new(raw.access_token),
                    token_type: raw.token_type,
                    scope: raw.scope,
                    refresh_token: raw.refresh_token.map(SecretString::new),
                    expires_in: raw.expires_in,
                });
            }
            // GitHub returns 200 + error body for pending authorization —
            // fall through to error handling.
        }

        let err: TokenErrorBody = serde_json::from_str(&text).map_err(|_| ProviderError::Api {
            status: status.as_u16(),
            body: text.clone(),
        })?;

        match err.error.as_str() {
            // User hasn't approved yet — keep polling at current interval.
            "authorization_pending" => continue,
            // Server says we're polling too fast — per RFC, +5s permanently.
            "slow_down" => {
                interval += Duration::from_secs(5);
                continue;
            }
            "access_denied" => return Err(ProviderError::Unauthorized),
            "expired_token" => {
                return Err(ProviderError::Api {
                    status: status.as_u16(),
                    body: "device code expired; restart the flow".into(),
                })
            }
            other => {
                return Err(ProviderError::Api {
                    status: status.as_u16(),
                    body: format!("oauth error: {other}"),
                })
            }
        }
    }
}

/// Public client_id for GitHub CLI's OAuth App.
///
/// GitHub has two flavours of authorisable identity:
///   * **OAuth Apps** — raw-hex client_id, broader scopes, user-controlled.
///   * **GitHub Apps** — `Iv1.…` prefix, per-installation permissions, app
///     publisher picks the scope allow-list.
///
/// We previously shipped `Iv1.b507a08c87ecfe98`, which is a *GitHub App*
/// (the "GitHub Copilot Plugin") and whose installation permissions cap
/// at `read:user`. Requesting `repo` against that client_id silently
/// succeeded but minted a `read:user`-only token — making every push
/// fail with 403. Switching to the `gh` CLI's OAuth App client_id lets
/// the `repo` scope grant actually take effect.
///
/// TODO: register a dedicated mergeFox OAuth App and move off gh CLI's
/// id (they're public, not secret, but using someone else's is poor
/// hygiene and leaves us stuck with their scope policy).
pub fn github_device_config() -> OAuthDeviceConfig {
    OAuthDeviceConfig {
        device_auth_url: "https://github.com/login/device/code".into(),
        token_url: "https://github.com/login/oauth/access_token".into(),
        client_id: "178c6fc778ccc68e1d6a".into(),
        scope: "repo read:org read:user".into(),
    }
}

/// Codeberg runs Gitea — device-flow-capable when the instance admin enables
/// it and registers an app. Users must supply their own client_id today;
/// we only expose the endpoint shape.
pub fn gitea_device_config(instance_base: &str, client_id: String) -> OAuthDeviceConfig {
    let base = instance_base.trim_end_matches('/');
    OAuthDeviceConfig {
        device_auth_url: format!("{base}/login/oauth/device/authorize"),
        token_url: format!("{base}/login/oauth/access_token"),
        client_id,
        // Gitea uses space-separated scopes; "write:repository" is enough for
        // clone/push. Upstream callers can override this if they need more.
        scope: "write:repository".into(),
    }
}
