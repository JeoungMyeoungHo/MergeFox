//! Protocol-agnostic AI client.
//!
//! Two implementations live here:
//!   * `OpenAICompatClient` — `/chat/completions`, used for OpenAI,
//!     Ollama, llama.cpp, LM Studio, vLLM and friends. Passes the
//!     optional `grammar` field for servers that honour it (flagged
//!     per endpoint via `supports_grammar`).
//!   * `AnthropicClient` — `/v1/messages`. Pulls the system prompt out
//!     of the message list into the top-level `system` field (that's
//!     where the Anthropic API wants it).
//!
//! Both go through a retry wrapper that backs off on 429/5xx — LLM
//! endpoints are flaky by nature (esp. cold local servers), and the
//! alternative of surfacing every transient error to the UI is noisy.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ai::config::{Endpoint, Protocol};
use crate::ai::error::{AiError, Result};

/// A chat message in our internal representation. Protocol adapters
/// translate this to/from the wire shape.
#[derive(Debug, Clone)]
pub struct Msg {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    fn as_openai_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// Everything a task needs to tell the client.
///
/// `grammar` and `json_schema` are both hints — a given endpoint may
/// support neither, one, or both. The client drops the one that
/// isn't supported silently; the task is responsible for including
/// redundant instructions in the system prompt for weaker providers.
#[derive(Debug, Clone, Default)]
pub struct CompletionRequest {
    pub system: String,
    pub messages: Vec<Msg>,
    pub max_tokens: u32,
    pub temperature: f32,
    /// GBNF source. Only honoured when `endpoint.supports_grammar`.
    pub grammar: Option<String>,
    /// JSON schema for providers that support structured outputs (e.g.
    /// OpenAI `response_format = json_schema`). Ignored when absent.
    pub json_schema: Option<Value>,
    /// Stop sequences — used by tasks whose GBNF would allow runaway.
    pub stop: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub text: String,
    pub finish_reason: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
}

/// Object-safe async trait.
///
/// We can't use `async fn` in traits on stable without sealing
/// downstream impls to a specific future, and the `async_trait` crate
/// isn't in our dep set. The hand-rolled form below returns a
/// `Pin<Box<dyn Future>>` — the same thing `async_trait` expands to.
pub trait AiClient: Send + Sync {
    fn complete<'a>(
        &'a self,
        req: CompletionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<CompletionResponse>> + Send + 'a>>;
}

/// Build the right client impl for an endpoint.
///
/// Boxed so callers can hold `Box<dyn AiClient>` without caring which
/// protocol variant they got.
pub fn build_client(endpoint: Endpoint) -> Box<dyn AiClient + Send + Sync> {
    let http = reqwest::Client::builder()
        // Local models can be slow to warm up; frontier APIs rarely
        // exceed 60s even for long outputs.
        .timeout(Duration::from_secs(120))
        .build()
        // If the TLS backend can't init there's nothing sane to do
        // at runtime — fall back to default which also can't fail in
        // practice because rustls is statically linked.
        .unwrap_or_else(|_| reqwest::Client::new());

    match endpoint.protocol {
        Protocol::OpenAICompatible => Box::new(OpenAICompatClient { endpoint, http }),
        Protocol::Anthropic => Box::new(AnthropicClient { endpoint, http }),
    }
}

// ---------- OpenAI-compatible -------------------------------------------

pub struct OpenAICompatClient {
    pub endpoint: Endpoint,
    pub http: reqwest::Client,
}

#[derive(Serialize)]
struct OaiChatReq<'a> {
    model: &'a str,
    messages: Vec<OaiMsg<'a>>,
    temperature: f32,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<&'a [String]>,
    // llama.cpp / Ollama extension — ignored by OpenAI itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    grammar: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<Value>,
}

#[derive(Serialize)]
struct OaiMsg<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Deserialize)]
struct OaiChatResp {
    choices: Vec<OaiChoice>,
    #[serde(default)]
    usage: Option<OaiUsage>,
}

#[derive(Deserialize)]
struct OaiChoice {
    message: OaiRespMsg,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct OaiRespMsg {
    #[serde(default)]
    content: String,
}

#[derive(Deserialize, Default)]
struct OaiUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

impl AiClient for OpenAICompatClient {
    fn complete<'a>(
        &'a self,
        req: CompletionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<CompletionResponse>> + Send + 'a>> {
        Box::pin(async move {
            let base = self.endpoint.base_url.trim_end_matches('/');
            let url = if base.ends_with("/v1") || base.ends_with("/v1/") {
                format!("{base}/chat/completions")
            } else {
                format!("{base}/v1/chat/completions")
            };

            // Translate our Msg list to the wire shape. We also inline
            // the system prompt as the first element since OpenAI-compat
            // servers expect it there, not as a separate field.
            let mut msgs: Vec<OaiMsg> = Vec::with_capacity(req.messages.len() + 1);
            if !req.system.is_empty() {
                msgs.push(OaiMsg {
                    role: "system",
                    content: &req.system,
                });
            }
            for m in &req.messages {
                msgs.push(OaiMsg {
                    role: m.role.as_openai_str(),
                    content: &m.content,
                });
            }

            let grammar = if self.endpoint.supports_grammar {
                req.grammar.as_deref()
            } else {
                None
            };

            // Prefer grammar when available — it's a hard constraint.
            // Fall back to response_format=json_object for providers
            // that advertise JSON mode (OpenAI 1106+). We don't detect
            // that automatically; tasks that need JSON always set
            // json_schema and we only use it when grammar is absent.
            let response_format = if grammar.is_none() && req.json_schema.is_some() {
                Some(json!({ "type": "json_object" }))
            } else {
                None
            };

            let body = OaiChatReq {
                model: &self.endpoint.model_id,
                messages: msgs,
                temperature: req.temperature,
                max_tokens: req.max_tokens.min(self.endpoint.max_output),
                stream: false,
                stop: if req.stop.is_empty() {
                    None
                } else {
                    Some(&req.stop)
                },
                grammar,
                response_format,
            };

            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            let key = self.endpoint.api_key.expose_secret();
            if !key.is_empty() {
                if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", key)) {
                    headers.insert(AUTHORIZATION, v);
                }
            }

            let resp = with_retry(|| async {
                self.http
                    .post(&url)
                    .headers(headers.clone())
                    .json(&body)
                    .send()
                    .await
                    .map_err(map_reqwest)
            })
            .await?;

            let status = resp.status();
            let text = resp.text().await.map_err(map_reqwest)?;
            if !status.is_success() {
                return Err(classify_status(status.as_u16(), text));
            }

            let parsed: OaiChatResp = serde_json::from_str(&text).map_err(|e| AiError::Parse {
                parser: format!("openai chat envelope: {}", e),
                raw: truncate_for_err(&text),
            })?;

            let choice = parsed.choices.into_iter().next().ok_or(AiError::Parse {
                parser: "openai: empty choices".into(),
                raw: truncate_for_err(&text),
            })?;

            let usage = parsed.usage.unwrap_or_default();
            Ok(CompletionResponse {
                text: choice.message.content,
                finish_reason: choice.finish_reason.unwrap_or_default(),
                tokens_in: usage.prompt_tokens,
                tokens_out: usage.completion_tokens,
            })
        })
    }
}

// ---------- Anthropic ---------------------------------------------------

pub struct AnthropicClient {
    pub endpoint: Endpoint,
    pub http: reqwest::Client,
}

#[derive(Serialize)]
struct AntReq<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    system: &'a str,
    messages: Vec<AntMsg<'a>>,
    max_tokens: u32,
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<&'a [String]>,
}

#[derive(Serialize)]
struct AntMsg<'a> {
    role: &'static str, // "user" | "assistant"
    content: &'a str,
}

#[derive(Deserialize)]
struct AntResp {
    #[serde(default)]
    content: Vec<AntContent>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<AntUsage>,
}

#[derive(Deserialize)]
struct AntContent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize, Default)]
struct AntUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

impl AiClient for AnthropicClient {
    fn complete<'a>(
        &'a self,
        req: CompletionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<CompletionResponse>> + Send + 'a>> {
        Box::pin(async move {
            let url = format!(
                "{}/v1/messages",
                self.endpoint.base_url.trim_end_matches('/')
            );

            // Anthropic's API disallows `system` role inside `messages`;
            // any System msg we carry gets folded into the top-level
            // `system` string alongside `req.system`.
            let mut system_prompt = req.system.clone();
            let mut msgs: Vec<AntMsg> = Vec::with_capacity(req.messages.len());
            for m in &req.messages {
                match m.role {
                    Role::System => {
                        if !system_prompt.is_empty() {
                            system_prompt.push_str("\n\n");
                        }
                        system_prompt.push_str(&m.content);
                    }
                    Role::User => msgs.push(AntMsg {
                        role: "user",
                        content: &m.content,
                    }),
                    Role::Assistant => msgs.push(AntMsg {
                        role: "assistant",
                        content: &m.content,
                    }),
                }
            }

            let body = AntReq {
                model: &self.endpoint.model_id,
                system: &system_prompt,
                messages: msgs,
                max_tokens: req.max_tokens.min(self.endpoint.max_output),
                temperature: req.temperature,
                stop_sequences: if req.stop.is_empty() {
                    None
                } else {
                    Some(&req.stop)
                },
            };

            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
            let key = self.endpoint.api_key.expose_secret();
            if !key.is_empty() {
                if let Ok(v) = HeaderValue::from_str(key) {
                    headers.insert("x-api-key", v);
                }
            }

            let resp = with_retry(|| async {
                self.http
                    .post(&url)
                    .headers(headers.clone())
                    .json(&body)
                    .send()
                    .await
                    .map_err(map_reqwest)
            })
            .await?;

            let status = resp.status();
            let text = resp.text().await.map_err(map_reqwest)?;
            if !status.is_success() {
                return Err(classify_status(status.as_u16(), text));
            }

            let parsed: AntResp = serde_json::from_str(&text).map_err(|e| AiError::Parse {
                parser: format!("anthropic envelope: {}", e),
                raw: truncate_for_err(&text),
            })?;

            // Anthropic returns a content *array* (text + future tool
            // uses). We concatenate all `text` blocks so a server that
            // chunks output doesn't drop content on the floor.
            let concat = parsed
                .content
                .iter()
                .filter(|c| c.kind == "text")
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join("");

            let usage = parsed.usage.unwrap_or_default();
            Ok(CompletionResponse {
                text: concat,
                finish_reason: parsed.stop_reason.unwrap_or_default(),
                tokens_in: usage.input_tokens,
                tokens_out: usage.output_tokens,
            })
        })
    }
}

// ---------- retry + error mapping --------------------------------------

/// Exponential backoff on transient HTTP errors.
///
/// Waits 100ms, 500ms, 2s between attempts (max 3 total). We don't
/// retry on 4xx other than 429 — they're deterministic failures.
async fn with_retry<F, Fut, T>(mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let waits = [
        Duration::from_millis(100),
        Duration::from_millis(500),
        Duration::from_secs(2),
    ];

    let mut last_err: Option<AiError> = None;
    // 3 attempts total: initial + 2 retries, matching waits[0..2] as
    // pre-retry delays for attempts 1 and 2.
    for attempt in 0..3 {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                // matches! with `if` only guards the whole pattern, and
                // a leading `|` prevents binding a variable — so we
                // split the HTTP-status branch into an explicit match.
                let retryable = match &e {
                    AiError::Network(_) | AiError::Timeout | AiError::RateLimited { .. } => true,
                    AiError::Http { status, .. } => *status >= 500,
                    _ => false,
                };
                last_err = Some(e);
                if !retryable || attempt == 2 {
                    break;
                }
                // Honour Retry-After on 429 if we have it; otherwise
                // fall back to the exponential ladder.
                let wait = if let Some(AiError::RateLimited {
                    retry_after: Some(ra),
                }) = &last_err
                {
                    *ra
                } else {
                    waits[attempt]
                };
                tokio::time::sleep(wait).await;
            }
        }
    }
    Err(last_err.unwrap_or(AiError::Network("retry exhausted".into())))
}

fn map_reqwest(e: reqwest::Error) -> AiError {
    if e.is_timeout() {
        AiError::Timeout
    } else {
        AiError::Network(e.to_string())
    }
}

fn classify_status(status: u16, body: String) -> AiError {
    match status {
        401 | 403 => AiError::Auth,
        429 => AiError::RateLimited { retry_after: None },
        // OpenAI-compat backends (LM Studio, llama.cpp, vLLM, …)
        // surface context-window overflow as a 400 with a body text
        // that reliably contains "context length" or "tokens to keep
        // from the initial prompt". Map those to the typed variant so
        // the UI can show an actionable "raise context window to N"
        // message instead of a raw "http 400: …" string.
        400 if body_looks_like_context_overflow(&body) => AiError::ContextOverflow {
            // We don't have precise counts from the endpoint; leave
            // them zero and let the caller's Display handler fall back
            // to generic advice when preflight didn't catch it first.
            used: 0,
            budget: 0,
        },
        _ => AiError::Http { status, body },
    }
}

fn body_looks_like_context_overflow(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("context length")
        || lower.contains("context_length")
        || lower.contains("tokens to keep")
        || lower.contains("maximum context")
        || lower.contains("context window")
        || lower.contains("too many tokens")
}

/// Keep error bodies readable in logs and `AiError::Parse.raw` without
/// dumping a 50KB HTML error page into the UI.
fn truncate_for_err(s: &str) -> String {
    const MAX: usize = 2048;
    if s.len() <= MAX {
        s.to_string()
    } else {
        let mut out = s[..MAX].to_string();
        out.push_str("... [truncated]");
        out
    }
}
