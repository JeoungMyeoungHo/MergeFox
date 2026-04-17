use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::journal::Journal;

use super::action_preview::{preview, ActionRequest};
use super::{view_for_repo, ActivityLogQuery};

pub fn run_stdio(repo_path: &Path) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();

    while let Some(message) = read_message(&mut reader)? {
        let request: RpcRequest = serde_json::from_slice(&message).context("decode JSON-RPC")?;
        if let Some(response) = handle_request(repo_path, request)? {
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

fn handle_request(repo_path: &Path, request: RpcRequest) -> Result<Option<RpcResponse>> {
    let Some(id) = request.id.clone() else {
        return Ok(None);
    };

    let response = match request.method.as_str() {
        "initialize" => ok(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": {
                    "name": "mergefox",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "tools": {
                        "listChanged": false
                    }
                }
            }),
        ),
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
                        "Dry-run a mergeFox action and classify its risk without executing it.",
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
                "mergefox_activity_log" => activity_log_tool(repo_path, arguments)?,
                "mergefox_action_preview" => action_preview_tool(repo_path, arguments)?,
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
