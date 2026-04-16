//! Task 2 — plain-language explanation of a commit or diff.
//!
//! Output is a markdown bullet list (max 8 bullets). We keep it tight
//! because (a) small models ramble and (b) the UI renders this in a
//! side panel where long prose scrolls off-screen.

use crate::ai::client::{AiClient, CompletionRequest, Msg, Role};
use crate::ai::diff_summarizer::summarize_for_prompt;
use crate::ai::error::{AiError, Result};
use crate::ai::grammars::GRAMMAR_EXPLAIN;

#[derive(Debug, Clone)]
pub struct Markdown {
    pub text: String,
}

/// Summarize a diff in bullet form.
///
/// `diff` may be a `git show` output for a commit or a plain diff —
/// we don't require one shape; the summarizer handles both.
pub async fn explain_change(client: &dyn AiClient, diff: &str) -> Result<Markdown> {
    if diff.trim().is_empty() {
        return Err(AiError::Parse {
            parser: "explain_change: empty input".into(),
            raw: String::new(),
        });
    }

    // Leave a little more room here than commit_message since we
    // want the model to understand the whole change.
    let trimmed = summarize_for_prompt(diff, 1800);

    let system = "You summarize git diffs for a reader who didn't write the code. Respond with \
         between 2 and 8 markdown bullet points. Each bullet starts with `- ` and fits on one \
         line. Do not include headings, code blocks, or prose before/after the list. Focus on \
         WHAT changed and WHY it probably changed — mention file names only when helpful.";

    let req = CompletionRequest {
        system: system.to_string(),
        messages: vec![Msg {
            role: Role::User,
            content: format!("Diff:\n\n{}", trimmed),
        }],
        max_tokens: 400,
        temperature: 0.3,
        grammar: Some(GRAMMAR_EXPLAIN.to_string()),
        json_schema: None,
        stop: vec![],
    };

    let resp = client.complete(req).await?;
    Ok(Markdown {
        text: clean_bullets(&resp.text),
    })
}

/// Trim to the bulleted region. Handles the case where a non-grammar
/// model prepends "Here are the changes:" before the list.
fn clean_bullets(text: &str) -> String {
    let mut out_lines: Vec<&str> = Vec::new();
    let mut seen_bullet = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let is_bullet = trimmed.starts_with("- ") || trimmed.starts_with("* ");
        if is_bullet {
            seen_bullet = true;
            // Normalize `* ` to `- `.
            if let Some(rest) = trimmed.strip_prefix("* ") {
                out_lines.push(rest);
                // We store the raw rest and rebuild below — avoids
                // holding a mutated String in the vec.
            } else {
                out_lines.push(trimmed);
            }
            if out_lines.len() >= 8 {
                break;
            }
        } else if seen_bullet && trimmed.is_empty() {
            break;
        }
        // Lines before the first bullet (preamble) are intentionally
        // skipped. Lines between bullets that aren't blank are
        // continuations of the prior bullet — we drop those because
        // the grammar forces single-line bullets and the UI column is
        // narrow.
    }

    if out_lines.is_empty() {
        // Fallback: the model gave us prose. Wrap each sentence as a
        // bullet so downstream UI always sees a list.
        return text
            .split_terminator('.')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .take(8)
            .map(|s| format!("- {}.", s))
            .collect::<Vec<_>>()
            .join("\n");
    }

    out_lines
        .into_iter()
        .map(|line| {
            if line.starts_with("- ") {
                line.to_string()
            } else {
                format!("- {}", line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
