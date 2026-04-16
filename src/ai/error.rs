//! Typed AI errors.
//!
//! These exist so callers (UI, retry wrapper, task impls) can branch on
//! failure mode without string-matching. `Parse` carries the raw model
//! output so we can show it to the user for debugging — tiny models
//! often produce *almost* correct output and a human can salvage it.

use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    /// Underlying transport failure (DNS, TLS, connect, read).
    #[error("network error: {0}")]
    Network(String),

    /// Non-2xx HTTP status. Body is captured for diagnostics.
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },

    /// 401/403 — almost always a missing or stale API key.
    #[error("authentication failed")]
    Auth,

    /// 429 or provider-specific rate-limit. `retry_after` is honoured
    /// by the retry wrapper if present.
    #[error("rate limited (retry_after={retry_after:?})")]
    RateLimited { retry_after: Option<Duration> },

    /// Request exceeded provider timeout.
    #[error("timed out")]
    Timeout,

    /// Prompt + diff larger than model's context window after trimming.
    /// Carries both numbers so the UI can say "used 8192, budget 4096".
    #[error("context overflow: used {used}, budget {budget}")]
    ContextOverflow { used: u32, budget: u32 },

    /// Model returned output we couldn't parse into the expected shape.
    /// `parser` describes which parser failed; `raw` is the literal
    /// text the model emitted (trimmed to a sensible size by callers).
    /// (Field is named `parser` rather than `source` to avoid thiserror's
    /// auto-chaining on a field named `source`.)
    #[error("parse failure ({parser}): {raw}")]
    Parse { parser: String, raw: String },

    /// User or caller aborted the operation.
    #[error("cancelled")]
    Cancelled,

    /// No endpoint configured / no AI provider wired up.
    #[error("no AI endpoint configured")]
    NotConfigured,
}

pub type Result<T> = std::result::Result<T, AiError>;
