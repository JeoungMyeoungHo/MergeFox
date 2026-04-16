//! Structured error type for provider REST interactions.
//!
//! We keep this separate from `anyhow::Error` at the provider boundary so
//! callers (UI, clone orchestration) can branch on the failure mode —
//! e.g. prompt for reauth on `Unauthorized`, back off on `RateLimited`.

use std::time::Duration;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("network error: {0}")]
    Network(String),

    /// Non-2xx response we couldn't map to a more specific variant.
    #[error("api error (status {status}): {body}")]
    Api { status: u16, body: String },

    #[error("failed to parse provider response: {0}")]
    Parse(String),

    #[error("not implemented for this provider yet: {0}")]
    NotImplemented(&'static str),

    /// HTTP 429 or provider-specific rate limit.
    #[error("rate limited (retry after {retry_after:?})")]
    RateLimited { retry_after: Option<Duration> },

    /// HTTP 401 — token missing, expired, or revoked. UI should offer reauth.
    #[error("unauthorized — token missing or expired")]
    Unauthorized,

    /// HTTP 404 — repo name might be wrong, or the token lacks visibility.
    #[error("not found — check the repo path or token scope")]
    NotFound,
}

impl From<reqwest::Error> for ProviderError {
    fn from(e: reqwest::Error) -> Self {
        ProviderError::Network(e.to_string())
    }
}

impl From<serde_json::Error> for ProviderError {
    fn from(e: serde_json::Error) -> Self {
        ProviderError::Parse(e.to_string())
    }
}

/// Shorthand for provider results. Not an alias for `anyhow::Result` because
/// we want the structured variants to survive back up to the UI.
pub type ProviderResult<T> = std::result::Result<T, ProviderError>;
