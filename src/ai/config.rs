//! Endpoint configuration.
//!
//! An `Endpoint` is a complete description of "where we send requests":
//! protocol, base URL, model, key, capabilities. We keep the API key
//! in `SecretString` and skip it from serde so the on-disk config file
//! never contains credentials — they live in the OS keyring instead.
//!
//! Presets exist so the UI can offer a one-click path for the common
//! cases (Ollama on localhost, OpenAI, Anthropic) without the user
//! having to hand-type base URLs.

use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// Wire protocol for a given endpoint.
///
/// We don't try to detect this from the URL — the same base URL can
/// speak either shape (e.g. some gateways), so the user picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    /// POST {base}/chat/completions — accepted by OpenAI, Ollama,
    /// llama.cpp server, vLLM, LM Studio, OpenRouter, etc.
    OpenAICompatible,
    /// POST {base}/v1/messages — Anthropic Messages API.
    Anthropic,
}

/// A single LLM endpoint we can target.
///
/// `api_key` is intentionally `#[serde(skip)]` — we persist everything
/// *except* the secret. On load, the app fetches the key from the OS
/// keyring using `name` as the account identifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    /// Human-visible label ("Local Qwen", "Claude Sonnet", ...).
    /// Also the keyring "user" for this endpoint's api_key.
    pub name: String,

    pub protocol: Protocol,

    /// e.g. `http://localhost:11434/v1` or `https://api.anthropic.com`.
    /// We don't append path segments here — clients own their route.
    pub base_url: String,

    /// Never persisted; filled from keyring after deserialize.
    #[serde(skip, default = "empty_secret")]
    pub api_key: SecretString,

    /// Provider-specific model id (e.g. `qwen2.5:0.5b`, `claude-opus-4-5`).
    pub model_id: String,

    /// Declared context window in tokens. Used by the diff summarizer
    /// to choose a trim budget — if we don't know, default to 4096.
    pub context_window: u32,

    /// Hard cap on `max_tokens` in requests to this endpoint.
    pub max_output: u32,

    /// Whether the OpenAI-compat endpoint honours the `grammar` field
    /// (llama.cpp and Ollama do; OpenAI itself does not). Anthropic
    /// endpoints ignore this flag.
    pub supports_grammar: bool,

    /// For future streaming UI; tasks don't currently depend on this.
    pub supports_streaming: bool,
}

fn empty_secret() -> SecretString {
    SecretString::new(String::new())
}

/// Ollama on localhost — the zero-config case.
///
/// Ollama exposes an OpenAI-compatible surface at port 11434 and
/// accepts `grammar` via its llama.cpp backend, so this preset flips
/// `supports_grammar = true`. No API key is required.
pub fn ollama_preset(model: &str) -> Endpoint {
    Endpoint {
        name: format!("ollama:{}", model),
        protocol: Protocol::OpenAICompatible,
        base_url: "http://localhost:11434/v1".to_string(),
        api_key: SecretString::new(String::new()),
        model_id: model.to_string(),
        // Conservative default — callers should bump this once they
        // know the actual model's declared context size.
        context_window: 4096,
        max_output: 512,
        supports_grammar: true,
        supports_streaming: true,
    }
}

/// OpenAI hosted API. No grammar support.
pub fn openai_preset(model: &str, key: SecretString) -> Endpoint {
    Endpoint {
        name: format!("openai:{}", model),
        protocol: Protocol::OpenAICompatible,
        base_url: "https://api.openai.com/v1".to_string(),
        api_key: key,
        model_id: model.to_string(),
        context_window: 128_000,
        max_output: 1024,
        supports_grammar: false,
        supports_streaming: true,
    }
}

/// Anthropic hosted API.
pub fn anthropic_preset(model: &str, key: SecretString) -> Endpoint {
    Endpoint {
        name: format!("anthropic:{}", model),
        protocol: Protocol::Anthropic,
        // Base only — the client appends `/v1/messages`.
        base_url: "https://api.anthropic.com".to_string(),
        api_key: key,
        model_id: model.to_string(),
        context_window: 200_000,
        max_output: 1024,
        supports_grammar: false,
        supports_streaming: true,
    }
}

/// Keyring lookup key for an endpoint's api_key.
///
/// Service is a fixed string so the OS keychain groups all our entries;
/// `endpoint_name` is the account. We keep this as a typed newtype so
/// call sites don't accidentally use a raw endpoint name for some other
/// keyring service (e.g. git provider tokens).
#[derive(Debug, Clone)]
pub struct EndpointKeyringKey {
    pub endpoint_name: String,
}

impl EndpointKeyringKey {
    pub const SERVICE: &'static str = "mergefox-ai";

    pub fn new(endpoint_name: impl Into<String>) -> Self {
        Self {
            endpoint_name: endpoint_name.into(),
        }
    }
}

/// Store an API key. Routes through the process-wide `SecretStore`
/// (in-memory cache + file backend). Errors if the store has not yet
/// been installed — only happens in tests or early-startup code paths
/// that try to save a secret before `MergeFoxApp::new` ran.
pub fn save_api_key(endpoint_name: &str, key: &SecretString) -> anyhow::Result<()> {
    use secrecy::ExposeSecret;
    let store = crate::secrets::SecretStore::global()
        .ok_or_else(|| anyhow::anyhow!("secret store not yet initialised"))?;
    let cred = crate::secrets::Credential::new(EndpointKeyringKey::SERVICE, endpoint_name);
    store.save(&cred, SecretString::new(key.expose_secret().clone()))
}

/// Fetch an API key. Returns an empty secret when no entry is stored,
/// or when the store hasn't been installed yet (tests/startup).
pub fn load_api_key(endpoint_name: &str) -> anyhow::Result<SecretString> {
    let Some(store) = crate::secrets::SecretStore::global() else {
        return Ok(SecretString::new(String::new()));
    };
    let cred = crate::secrets::Credential::new(EndpointKeyringKey::SERVICE, endpoint_name);
    Ok(store
        .load(&cred)?
        .unwrap_or_else(|| SecretString::new(String::new())))
}

/// Remove the stored key (used when the user deletes/renames an endpoint).
#[allow(dead_code)]
pub fn delete_api_key(endpoint_name: &str) -> anyhow::Result<()> {
    let Some(store) = crate::secrets::SecretStore::global() else {
        return Ok(());
    };
    let cred = crate::secrets::Credential::new(EndpointKeyringKey::SERVICE, endpoint_name);
    store.delete(&cred)
}
