//! Task 4 — merge-conflict resolution suggestion.
//!
//! Given base / ours / theirs hunks, the model proposes one of:
//!   * `ours`   — keep our side verbatim;
//!   * `theirs` — keep their side verbatim;
//!   * `merged` — a hand-merged block; `merged_text` holds the result.
//!
//! This is the hardest task on the list — tiny models will often
//! hallucinate code that doesn't compile. We surface `rationale` so
//! the UI can show its work; callers MUST treat `merged_text` as a
//! suggestion, not a drop-in replacement.

use serde::Deserialize;
use serde_json::json;

use crate::ai::client::{AiClient, CompletionRequest, Msg, Role};
use crate::ai::error::{AiError, Result};
use crate::ai::grammars::GRAMMAR_CONFLICT;

#[derive(Debug, Clone)]
pub struct ConflictResolution {
    pub resolution: Resolution,
    pub merged_text: String,
    pub rationale: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Ours,
    Theirs,
    Merged,
}

#[derive(Deserialize)]
struct Wire {
    resolution: String,
    #[serde(default)]
    merged_text: String,
    #[serde(default)]
    rationale: String,
}

/// Ask the model to pick a side (or merge) for a single conflict hunk.
pub async fn suggest_conflict_resolution(
    client: &dyn AiClient,
    base: &str,
    ours: &str,
    theirs: &str,
) -> Result<ConflictResolution> {
    // We don't trim these — conflict hunks are usually small, and
    // losing context here risks a worse suggestion. Callers that
    // have huge hunks should split them before calling us.
    let user =
        format!("# Base (common ancestor)\n\n{base}\n\n# Ours\n\n{ours}\n\n# Theirs\n\n{theirs}\n");

    let system = "You resolve git merge conflicts. Respond with a single JSON object matching \
         exactly this schema: {\"resolution\": \"ours\"|\"theirs\"|\"merged\", \"merged_text\": \
         string, \"rationale\": string}. If `resolution` is \"ours\" or \"theirs\" copy that side \
         verbatim into `merged_text`. If \"merged\", produce text that preserves both sides' \
         intent. The rationale must be at most two sentences. Do not wrap the JSON in code fences.";

    // For non-grammar endpoints we also pass a JSON schema hint so
    // OpenAI-style `response_format: json_object` can kick in.
    let schema = json!({
        "type": "object",
        "properties": {
            "resolution": { "enum": ["ours", "theirs", "merged"] },
            "merged_text": { "type": "string" },
            "rationale": { "type": "string" }
        },
        "required": ["resolution", "merged_text", "rationale"]
    });

    let req = CompletionRequest {
        system: system.to_string(),
        messages: vec![Msg {
            role: Role::User,
            content: user,
        }],
        max_tokens: 1024,
        temperature: 0.1,
        grammar: Some(GRAMMAR_CONFLICT.to_string()),
        json_schema: Some(schema),
        stop: vec![],
    };

    let resp = client.complete(req).await?;
    parse_resolution(&resp.text)
}

fn parse_resolution(text: &str) -> Result<ConflictResolution> {
    // Tiny models sometimes put the JSON inside a fence or after a
    // preamble. Locate the first `{` and last `}` and try that window.
    let trimmed = text.trim();
    let json_slice = extract_json_object(trimmed).ok_or_else(|| AiError::Parse {
        parser: "conflict: no JSON object found".into(),
        raw: trimmed.chars().take(1024).collect(),
    })?;

    let w: Wire = serde_json::from_str(json_slice).map_err(|e| AiError::Parse {
        parser: format!("conflict: {}", e),
        raw: json_slice.chars().take(1024).collect(),
    })?;

    let resolution = match w.resolution.as_str() {
        "ours" => Resolution::Ours,
        "theirs" => Resolution::Theirs,
        "merged" => Resolution::Merged,
        other => {
            return Err(AiError::Parse {
                parser: format!("conflict: unknown resolution `{}`", other),
                raw: json_slice.chars().take(1024).collect(),
            })
        }
    };

    Ok(ConflictResolution {
        resolution,
        merged_text: w.merged_text,
        rationale: w.rationale,
    })
}

/// Return the substring from the first `{` to the matching closing
/// brace. Not a full JSON parser — it just tracks depth ignoring
/// braces inside strings. Good enough for LLM output that's
/// structurally but not positionally clean.
pub(crate) fn extract_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for i in start..bytes.len() {
        let b = bytes[i];
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_json() {
        let raw = r#"{"resolution":"ours","merged_text":"foo","rationale":"keep X"}"#;
        let r = parse_resolution(raw).unwrap();
        assert_eq!(r.resolution, Resolution::Ours);
        assert_eq!(r.merged_text, "foo");
    }

    #[test]
    fn parses_fenced_json() {
        let raw = "Here you go:\n```json\n{\"resolution\":\"merged\",\"merged_text\":\"a\\nb\",\"rationale\":\"combine\"}\n```";
        let r = parse_resolution(raw).unwrap();
        assert_eq!(r.resolution, Resolution::Merged);
    }
}
