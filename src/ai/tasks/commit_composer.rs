//! Task 5 — split a dirty tree into a logical commit sequence.
//!
//! Given a big diff, the model proposes N commits, each with a title,
//! body, and the list of files that belong in it. Output is strict
//! JSON so we can drive the UI directly (render proposed commits;
//! user picks one and we stage+commit those files).
//!
//! This task asks more reasoning of the model than the others —
//! expect frontier models to behave well, local 0.5B models to be
//! only marginally useful. We clamp `max_commits` on the way in and
//! validate files on the way out.

use serde::Deserialize;
use serde_json::json;

use crate::ai::client::{AiClient, CompletionRequest, Msg, Role};
use crate::ai::diff_summarizer::summarize_for_prompt;
use crate::ai::error::{AiError, Result};
use crate::ai::grammars::GRAMMAR_COMPOSER;
use crate::ai::tasks::pr_conflict::extract_json_object;

#[derive(Debug, Clone)]
pub struct ComposerPlan {
    pub commits: Vec<PlannedCommit>,
}

#[derive(Debug, Clone)]
pub struct PlannedCommit {
    pub title: String,
    pub body: String,
    pub files: Vec<String>,
}

#[derive(Deserialize)]
struct WirePlan {
    commits: Vec<WireCommit>,
}

#[derive(Deserialize)]
struct WireCommit {
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    files: Vec<String>,
}

/// Produce a plan that splits `diff` into up to `max_commits` commits.
///
/// We never fewer than 1 or more than 10 — enforced because a runaway
/// model can otherwise propose hundreds.
pub async fn compose_commits(
    client: &dyn AiClient,
    diff: &str,
    max_commits: u32,
) -> Result<ComposerPlan> {
    if diff.trim().is_empty() {
        return Err(AiError::Parse {
            parser: "composer: empty diff".into(),
            raw: String::new(),
        });
    }
    let cap = max_commits.clamp(1, 10);

    // Composer needs the full list of changed files — we give it a
    // generous budget. If we have to drop file bodies, the summarizer
    // still emits the "…N more files omitted (paths: a, b)" footer so
    // the model can at least put those paths in *some* commit.
    let trimmed = summarize_for_prompt(diff, 2400);

    let system = format!(
        "You group git changes into a clean commit history. Respond with a single JSON object \
         matching exactly: {{\"commits\":[{{\"title\":string,\"body\":string,\"files\":[string,...]}},...]}}. \
         Produce between 1 and {cap} commits. Every file that appears in the diff must appear in \
         exactly one commit. Titles follow Conventional Commits (`feat:`, `fix:`, ...) and are \
         under 72 characters. Body is one paragraph or empty string. Do not wrap the JSON in code \
         fences."
    );

    let schema = json!({
        "type": "object",
        "properties": {
            "commits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string" },
                        "body": { "type": "string" },
                        "files": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["title", "body", "files"]
                }
            }
        },
        "required": ["commits"]
    });

    let req = CompletionRequest {
        system,
        messages: vec![Msg {
            role: Role::User,
            content: format!("Diff:\n\n{}", trimmed),
        }],
        // Composer output can be large (N commits × prose + file lists).
        // 2048 is enough for ~6 sizeable commits; callers with big
        // trees should raise `endpoint.max_output` first.
        max_tokens: 2048,
        temperature: 0.2,
        grammar: Some(GRAMMAR_COMPOSER.to_string()),
        json_schema: Some(schema),
        stop: vec![],
    };

    let resp = client.complete(req).await?;
    let plan = parse_plan(&resp.text, cap)?;
    Ok(plan)
}

fn parse_plan(text: &str, cap: u32) -> Result<ComposerPlan> {
    let window = extract_json_object(text.trim()).ok_or_else(|| AiError::Parse {
        parser: "composer: no JSON object".into(),
        raw: text.chars().take(2048).collect(),
    })?;

    let mut w: WirePlan = serde_json::from_str(window).map_err(|e| AiError::Parse {
        parser: format!("composer: {}", e),
        raw: window.chars().take(2048).collect(),
    })?;

    // Enforce the cap post-hoc — a non-grammar model may still return
    // more. We truncate rather than error because a partial plan is
    // more useful than none, and the UI displays it as a suggestion.
    w.commits.truncate(cap as usize);

    let commits: Vec<PlannedCommit> = w
        .commits
        .into_iter()
        .filter_map(|c| {
            // Drop commits with empty title or no files — they're
            // always a model mistake and would confuse the UI.
            if c.title.trim().is_empty() || c.files.is_empty() {
                None
            } else {
                Some(PlannedCommit {
                    title: c.title.trim().to_string(),
                    body: c.body.trim().to_string(),
                    files: c
                        .files
                        .into_iter()
                        .map(|f| f.trim().to_string())
                        .filter(|f| !f.is_empty())
                        .collect(),
                })
            }
        })
        .collect();

    if commits.is_empty() {
        return Err(AiError::Parse {
            parser: "composer: no valid commits after filtering".into(),
            raw: window.chars().take(1024).collect(),
        });
    }

    Ok(ComposerPlan { commits })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_plan() {
        let raw = r#"{"commits":[{"title":"feat: x","body":"","files":["src/a.rs","src/b.rs"]}]}"#;
        let p = parse_plan(raw, 5).unwrap();
        assert_eq!(p.commits.len(), 1);
        assert_eq!(p.commits[0].files.len(), 2);
    }

    #[test]
    fn caps_overlong_plans() {
        let mut items = Vec::new();
        for i in 0..20 {
            items.push(format!(
                r#"{{"title":"feat: {i}","body":"","files":["f{i}"]}}"#
            ));
        }
        let raw = format!(r#"{{"commits":[{}]}}"#, items.join(","));
        let p = parse_plan(&raw, 3).unwrap();
        assert_eq!(p.commits.len(), 3);
    }

    #[test]
    fn drops_empty_commits() {
        let raw = r#"{"commits":[{"title":"","body":"","files":["a"]},{"title":"ok","body":"","files":["b"]}]}"#;
        let p = parse_plan(raw, 5).unwrap();
        assert_eq!(p.commits.len(), 1);
        assert_eq!(p.commits[0].title, "ok");
    }
}
