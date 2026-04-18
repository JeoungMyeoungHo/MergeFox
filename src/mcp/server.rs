use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::journal::Journal;

use super::action_preview::{preview, ActionRequest};
use super::{view_for_repo, ActivityLogQuery};

/// Session state tracked across the lifetime of one `run_stdio` call.
/// The authentication token is read from the `MERGEFOX_MCP_TOKEN`
/// environment variable. When set, every `tools/*` request must pass
/// the token in its params as `session_token`. When the env var is
/// empty (or unset) the server runs unauthenticated — that mode is
/// fine for local development but must NEVER be the default when
/// exposing MCP outside the current user session.
///
/// Token handshake is intentionally minimal: no expiry, no rotation,
/// no revocation list. A single shared secret keeps the attack surface
/// small while we're still iterating on the tool shape.
struct SessionCtx {
    repo_path: PathBuf,
    required_token: Option<String>,
    /// Set to `true` once the client has presented a matching token in
    /// `initialize`. After that we stop re-checking on each call — the
    /// stdio stream is point-to-point with the authenticated client,
    /// and re-validating adds no security.
    authenticated: bool,
}

pub fn run_stdio(repo_path: &Path) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();

    // Token source, in priority order:
    //   1. `MERGEFOX_MCP_TOKEN` env var (standard MCP client config)
    //   2. the UI-managed session token inside secrets.json (same
    //      machine, same user — matches what Settings → Integrations
    //      → MCP displays)
    // When neither is set, the server runs unauthenticated with a
    // loud warning. This is deliberately only usable by the local
    // user; the stdio pipe terminates inside their shell.
    let required_token = std::env::var("MERGEFOX_MCP_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let store = crate::secrets::SecretStore::new(crate::secrets::default_file_path());
            use secrecy::ExposeSecret;
            store
                .load_or_generate_mcp_token()
                .ok()
                .map(|tok| tok.expose_secret().to_string())
        });
    let mut ctx = SessionCtx {
        repo_path: repo_path.to_path_buf(),
        authenticated: required_token.is_none(),
        required_token,
    };
    if ctx.required_token.is_none() {
        tracing::warn!(
            target: "mergefox::mcp",
            "MCP stdio server running UNAUTHENTICATED (MERGEFOX_MCP_TOKEN unset). \
             Only safe on a private stdio pipe; never expose over a socket."
        );
    } else {
        tracing::info!(target: "mergefox::mcp", "MCP stdio server started (token auth)");
    }

    while let Some(message) = read_message(&mut reader)? {
        let request: RpcRequest = serde_json::from_slice(&message).context("decode JSON-RPC")?;
        if let Some(response) = handle_request(&mut ctx, request)? {
            write_message(&mut writer, &response)?;
            writer.flush().ok();
        }
    }

    Ok(())
}

#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i64,
    message: String,
}

fn handle_request(ctx: &mut SessionCtx, request: RpcRequest) -> Result<Option<RpcResponse>> {
    let Some(id) = request.id.clone() else {
        return Ok(None);
    };

    // Gate every tool method (but NOT `initialize` / `ping`) behind
    // token auth. Clients are expected to pass the token during
    // `initialize` under `clientInfo.session_token` OR as a top-level
    // `session_token` field — we accept both so the same protocol
    // works with strict MCP clients and ad-hoc callers.
    let method = request.method.as_str();
    let needs_auth = matches!(method, "tools/list" | "tools/call");
    if needs_auth && !ctx.authenticated {
        return Ok(Some(err(
            id,
            -32001,
            "missing or invalid session token; send `initialize` with session_token first",
        )));
    }

    let response = match method {
        "initialize" => {
            // Try to read a token from either `session_token` at top
            // level or under `clientInfo`. Mismatches reject the
            // handshake rather than ignoring it silently.
            if let Some(required) = ctx.required_token.as_ref() {
                let provided = request
                    .params
                    .as_ref()
                    .and_then(|p| {
                        p.get("session_token").and_then(Value::as_str).or_else(|| {
                            p.get("clientInfo")
                                .and_then(|ci| ci.get("session_token"))
                                .and_then(Value::as_str)
                        })
                    })
                    .unwrap_or("");
                if provided == required {
                    ctx.authenticated = true;
                } else {
                    return Ok(Some(err(
                        id,
                        -32002,
                        "session token does not match — start the host app and copy the token from Settings → Integrations → MCP",
                    )));
                }
            }
            ok(
                id,
                json!({
                    "protocolVersion": "2024-11-05",
                    "serverInfo": {
                        "name": "mergefox",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": {
                        "tools": { "listChanged": false }
                    }
                }),
            )
        }
        "ping" => ok(id, json!({})),
        "tools/list" => ok(
            id,
            json!({
                "tools": [
                    tool_descriptor(
                        "mergefox_activity_log",
                        "Return the recent mergeFox activity log for this repository.",
                        json!({
                            "type": "object",
                            "properties": {
                                "limit": { "type": "integer", "minimum": 1, "maximum": 500 },
                                "only_kind": { "type": "string" },
                                "only_source": { "type": "string", "enum": ["ui", "mcp", "external"] }
                            }
                        })
                    ),
                    tool_descriptor(
                        "mergefox_action_preview",
                        "Dry-run a mergeFox action and classify its risk without executing it. \
                         Safe to call on any action; never mutates the repo.",
                        json!({
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string" }
                            },
                            "required": ["kind"]
                        })
                    ),
                    tool_descriptor(
                        "mergefox_action_execute",
                        "Execute a mergeFox action. By default this REFUSES — requests require \
                         UI approval from the running mergeFox app, or an auto-approve tier \
                         opt-in via `MERGEFOX_MCP_AUTO_APPROVE` (safe | recoverable | all). \
                         Destructive actions additionally require `MERGEFOX_MCP_ALLOW_DESTRUCTIVE=1`.",
                        json!({
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string" }
                            },
                            "required": ["kind"]
                        })
                    )
                ]
            }),
        ),
        "tools/call" => {
            let params = request.params.unwrap_or_default();
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("tools/call requires `name`"))?;
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let result = match name {
                "mergefox_activity_log" => activity_log_tool(&ctx.repo_path, arguments)?,
                "mergefox_action_preview" => action_preview_tool(&ctx.repo_path, arguments)?,
                "mergefox_action_execute" => action_execute_tool(&ctx.repo_path, arguments)?,
                other => {
                    return Ok(Some(err(
                        id,
                        -32601,
                        format!("unknown mergefox tool `{other}`"),
                    )));
                }
            };
            ok(id, result)
        }
        "notifications/initialized" => return Ok(None),
        other => err(id, -32601, format!("unknown method `{other}`")),
    };

    Ok(Some(response))
}

fn tool_descriptor(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema
    })
}

fn activity_log_tool(repo_path: &Path, arguments: Value) -> Result<Value> {
    #[derive(Debug, Deserialize)]
    struct Args {
        limit: Option<usize>,
        only_kind: Option<String>,
        only_source: Option<String>,
    }

    let args: Args = serde_json::from_value(arguments).context("activity log arguments")?;
    let repo = crate::git::Repo::open(repo_path)?;
    let journal = Journal::load_or_init(repo.gix().git_dir())?;
    let view = view_for_repo(
        &journal,
        ActivityLogQuery {
            limit: args.limit.unwrap_or(50).clamp(1, 500),
            only_kind: args.only_kind,
            only_source: args.only_source,
        },
    );
    Ok(tool_result(json!({
        "repo_path": repo.path(),
        "total": view.total,
        "cursor": view.cursor,
        "entries": view.entries
    })))
}

fn action_preview_tool(repo_path: &Path, arguments: Value) -> Result<Value> {
    let action: ActionRequest = serde_json::from_value(arguments).context("action preview args")?;
    let preview = preview(repo_path, action)?;
    Ok(tool_result(json!(preview)))
}

/// Auto-approve tier: caller declares "I'm willing to auto-execute
/// actions up to tier X" via `MERGEFOX_MCP_AUTO_APPROVE`. Defaults to
/// `none` — **every** execute request is refused with a clear message.
/// Intentionally conservative: MCP servers run unattended; the user
/// opts into broader capability deliberately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoApproveTier {
    /// No execute: preview-only server. Default.
    None,
    /// Safe actions auto-execute (copy SHA, create branch, create tag).
    Safe,
    /// Safe + Recoverable (checkout, cherry-pick, stash).
    Recoverable,
    /// Everything — still subject to `MERGEFOX_MCP_ALLOW_DESTRUCTIVE=1`
    /// for the Destructive tier specifically, because "all" without
    /// that extra opt-in has been a common foot-gun in other agent
    /// tools.
    All,
}

fn auto_approve_tier() -> AutoApproveTier {
    match std::env::var("MERGEFOX_MCP_AUTO_APPROVE")
        .ok()
        .as_deref()
        .map(str::trim)
    {
        Some("safe") => AutoApproveTier::Safe,
        Some("recoverable") => AutoApproveTier::Recoverable,
        Some("all") => AutoApproveTier::All,
        _ => AutoApproveTier::None,
    }
}

fn destructive_allowed() -> bool {
    std::env::var("MERGEFOX_MCP_ALLOW_DESTRUCTIVE")
        .ok()
        .map(|v| v.trim() == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn action_execute_tool(repo_path: &Path, arguments: Value) -> Result<Value> {
    use crate::mcp::action_preview::ActionRisk;
    let action: ActionRequest = serde_json::from_value(arguments).context("action execute args")?;
    let preview_info = preview(repo_path, action.clone())?;

    let tier = auto_approve_tier();
    let allowed = match (preview_info.risk, tier) {
        (ActionRisk::Safe, AutoApproveTier::None) => false,
        (ActionRisk::Safe, _) => true,
        (ActionRisk::Recoverable, AutoApproveTier::Recoverable | AutoApproveTier::All) => true,
        (ActionRisk::Destructive, AutoApproveTier::All) => destructive_allowed(),
        _ => false,
    };

    if !allowed {
        let hint = match (preview_info.risk, tier) {
            (ActionRisk::Destructive, AutoApproveTier::All) if !destructive_allowed() => {
                "Destructive actions require `MERGEFOX_MCP_ALLOW_DESTRUCTIVE=1` in addition to \
                 `MERGEFOX_MCP_AUTO_APPROVE=all`."
            }
            (_, AutoApproveTier::None) => {
                "This server is preview-only. Set `MERGEFOX_MCP_AUTO_APPROVE=safe` (or \
                 `recoverable` / `all`) in the client config to opt into execution."
            }
            _ => {
                "Risk tier exceeds the configured auto-approve level. Raise it via \
                  `MERGEFOX_MCP_AUTO_APPROVE=recoverable` or `all`."
            }
        };
        return Ok(tool_result(json!({
            "executed": false,
            "reason": "approval_required",
            "message": hint,
            "preview": preview_info
        })));
    }

    // Execute. We only implement the small set of write ops we're
    // confident in from a headless context; the rest still route
    // through the UI where the user can intervene.
    let outcome = execute_approved_action(repo_path, &action)?;
    Ok(tool_result(json!({
        "executed": true,
        "risk": preview_info.risk,
        "message": outcome,
        "preview": preview_info
    })))
}

/// Actually run the action. Kept short on purpose — write ops here
/// run unattended, so we restrict the surface to reads + the handful
/// of writes that can't accidentally lose work (fetch, create-branch,
/// create-tag). Anything else returns a clear "needs UI" error even
/// when auto-approve nominally allows it.
fn execute_approved_action(repo_path: &Path, action: &ActionRequest) -> Result<String> {
    use crate::mcp::action_preview::ActionRequest as A;
    let repo = crate::git::Repo::open(repo_path)?;
    match action {
        A::CopySha { oid } | A::CopyShortSha { oid } => {
            // Clipboard access needs a UI thread on some platforms.
            // From a headless MCP context we just return the value.
            Ok(oid.clone())
        }
        A::CreateBranch { at, name } => {
            let branch_name = name
                .as_deref()
                .ok_or_else(|| anyhow!("create_branch requires `name`"))?;
            let at_oid = gix::ObjectId::from_hex(at.as_bytes())
                .map_err(|e| anyhow!("invalid SHA in `at`: {e}"))?;
            repo.create_branch(branch_name, at_oid)?;
            Ok(format!("Created branch {branch_name} at {at}"))
        }
        _ => Err(anyhow!(
            "this action is not yet executable via MCP — use the mergeFox UI for confirmation. \
             (implemented subset: create_branch, copy_sha)"
        )),
    }
}

fn tool_result(payload: Value) -> Value {
    let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into());
    json!({
        "content": [
            {
                "type": "text",
                "text": text
            }
        ],
        "structuredContent": payload,
        "isError": false
    })
}

fn ok(id: Value, result: Value) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

fn err(id: Value, code: i64, message: impl Into<String>) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(RpcError {
            code,
            message: message.into(),
        }),
    }
}

fn read_message(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();

    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = Some(value.trim().parse().context("invalid Content-Length")?);
            }
        }
    }

    let len = content_length.ok_or_else(|| anyhow!("missing Content-Length header"))?;
    let mut body = vec![0_u8; len];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

fn write_message(writer: &mut impl Write, response: &RpcResponse) -> Result<()> {
    let body = serde_json::to_vec(response)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    Ok(())
}

pub fn repo_path_from_args(args: &[String]) -> Result<Option<PathBuf>> {
    let mut repo_path: Option<PathBuf> = None;
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--mcp-stdio" => {}
            "--repo" => {
                idx += 1;
                let path = args
                    .get(idx)
                    .ok_or_else(|| anyhow!("`--repo` requires a path"))?;
                repo_path = Some(PathBuf::from(path));
            }
            "--help" | "-h" => {
                print_help();
                return Ok(None);
            }
            other => anyhow::bail!("unknown argument `{other}`"),
        }
        idx += 1;
    }
    Ok(Some(repo_path.unwrap_or(
        std::env::current_dir().context("current directory")?,
    )))
}

pub fn print_help() {
    println!(
        "\
mergefox

Usage:
  mergefox
  mergefox --mcp-stdio [--repo <path>]
"
    );
}

#[cfg(test)]
mod tests {
    use super::{read_message, write_message, RpcResponse};

    #[test]
    fn json_rpc_stdio_round_trip() {
        let response = RpcResponse {
            jsonrpc: "2.0",
            id: serde_json::json!(1),
            result: Some(serde_json::json!({"ok": true})),
            error: None,
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &response).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let body = read_message(&mut cursor).unwrap().unwrap();
        let decoded: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(decoded["id"], 1);
        assert_eq!(decoded["result"]["ok"], true);
    }
}
