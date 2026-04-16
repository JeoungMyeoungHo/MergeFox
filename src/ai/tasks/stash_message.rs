//! Task 3 — one-line stash label for a dirty working tree.
//!
//! Output is a single line, <=60 chars. No punctuation at the end (git
//! stash labels read awkwardly with a trailing period), no quotes.

use crate::ai::client::{AiClient, CompletionRequest, Msg, Role};
use crate::ai::diff_summarizer::summarize_for_prompt;
use crate::ai::error::{AiError, Result};
use crate::ai::grammars::GRAMMAR_STASH_MSG;

/// Generate a stash label from a dirty-tree diff.
pub async fn gen_stash_message(client: &dyn AiClient, diff: &str) -> Result<String> {
    if diff.trim().is_empty() {
        return Err(AiError::Parse {
            parser: "stash_message: empty diff".into(),
            raw: String::new(),
        });
    }

    // Stash labels don't need full diff fidelity — 600 tokens is enough
    // for the model to identify the theme.
    let trimmed = summarize_for_prompt(diff, 600);

    let system = "You write one-line labels for git stashes. Respond with EXACTLY one line, \
         under 60 characters, describing the in-progress work. No quotes, no trailing period, no \
         preamble. Prefer an imperative noun phrase like `rename auth helpers` or `WIP: conflict \
         panel layout` over full sentences.";

    let req = CompletionRequest {
        system: system.to_string(),
        messages: vec![Msg {
            role: Role::User,
            content: format!("Dirty working tree:\n\n{}", trimmed),
        }],
        max_tokens: 40,
        temperature: 0.4,
        grammar: Some(GRAMMAR_STASH_MSG.to_string()),
        json_schema: None,
        // Belt-and-braces: even with the grammar, a weak model that
        // ignores grammar will get cut off at the first newline.
        stop: vec!["\n".to_string()],
    };

    let resp = client.complete(req).await?;
    Ok(clean_label(&resp.text))
}

/// Strip quotes/fences and clamp to 60 chars.
fn clean_label(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    // Drop surrounding quotes, backticks, or code fences.
    for ch in ['"', '\'', '`'] {
        if s.starts_with(ch) && s.ends_with(ch) && s.len() >= 2 {
            s = s[1..s.len() - 1].to_string();
        }
    }
    // Take only the first line — a model that ignored our `\n` stop
    // might still emit multiple.
    if let Some(nl) = s.find('\n') {
        s.truncate(nl);
    }
    // Drop trailing period.
    while s.ends_with('.') {
        s.pop();
    }
    // Clamp to 60 chars (char boundary-safe).
    if s.chars().count() > 60 {
        s = s.chars().take(60).collect();
    }
    s.trim().to_string()
}
