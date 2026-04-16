//! AI task harness — wraps LLMs behind a narrow, typed interface.
//!
//! The goal is to make every AI-assisted feature of mergeFox usable on
//! machines that can *only* run a sub-1B local model (e.g. Qwen2.5-0.5B
//! via Ollama), while still getting good behaviour from frontier models
//! when users point us at one.
//!
//! To achieve that we:
//!   * speak both OpenAI-compatible and Anthropic Messages shapes so the
//!     same task module works across providers;
//!   * ship GBNF grammars for llama.cpp/Ollama so tiny models are forced
//!     into the right output shape, and fall back to JSON-schema prompts
//!     + regex parsing for providers that don't accept `grammar`;
//!   * aggressively trim diffs before prompting (see `diff_summarizer`)
//!     because context overflow is the #1 failure mode on local models;
//!   * never panic on malformed LLM output — parse failures surface as
//!     typed `AiError::Parse` so the UI can retry or degrade gracefully.
//!
//! The UI is synchronous (egui redraws on one thread); async HTTP lives
//! on a single shared tokio runtime (`runtime`). Each task is an
//! `async fn` that the UI dispatches via `runtime::block_on` or via a
//! background task whose result drops back into app state.

pub mod client;
pub mod config;
pub mod diff_summarizer;
pub mod error;
pub mod grammars;
pub mod runtime;
pub mod tasks;

// Re-exports — callers should touch these, not the inner modules, so
// we can rearrange internals without churning `app.rs`.
pub use client::{build_client, AiClient, CompletionRequest, CompletionResponse, Msg, Role};
pub use config::{
    anthropic_preset, load_api_key, ollama_preset, openai_preset, save_api_key, Endpoint, Protocol,
};
pub use error::AiError;
pub use runtime::AiTask;
pub use tasks::{
    commit_composer::{compose_commits, ComposerPlan, PlannedCommit},
    commit_message::{gen_commit_message, CommitMessageOpts, CommitSuggestion},
    explain_change::{explain_change, Markdown},
    pr_conflict::{suggest_conflict_resolution, ConflictResolution},
    stash_message::gen_stash_message,
};
