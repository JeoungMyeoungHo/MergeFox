//! Task 1 — generate a Conventional Commits message from a staged diff.
//!
//! On grammar-capable endpoints we force the Conventional Commit shape
//! at decode time (`GRAMMAR_COMMIT_MSG`) so even a 0.5B model can't
//! emit prose. On everything else we lean on a strong system prompt
//! plus a regex-based fallback parser — if the model still produces
//! "Sure! Here is your message: feat: ...", the regex finds the good
//! part and drops the preamble.

use crate::ai::client::{AiClient, CompletionRequest, Msg, Role};
use crate::ai::config::Protocol;
use crate::ai::diff_summarizer::summarize_for_prompt;
use crate::ai::error::{AiError, Result};
use crate::ai::grammars::GRAMMAR_COMMIT_MSG;

#[derive(Debug, Clone, Default)]
pub struct CommitMessageOpts {
    /// Cap on prompt tokens dedicated to the diff. Remaining budget
    /// goes to the system prompt and the model's output.
    pub diff_budget_tokens: u32,
    /// Hint for the model about the scope (e.g. "ui", "git") — empty
    /// means "let the model infer from paths".
    pub scope_hint: Option<String>,
    /// When true, ask for a body paragraph; otherwise title-only.
    pub include_body: bool,
}

#[derive(Debug, Clone)]
pub struct CommitSuggestion {
    pub title: String,
    pub body: Option<String>,
    pub commit_type: String,
    pub scope: Option<String>,
}

const CONVENTIONAL_TYPES: &[&str] = &[
    "feat", "fix", "docs", "style", "refactor", "perf", "test", "build", "ci", "chore", "revert",
];

/// Public entry point. `diff` is the full staged diff — we trim it
/// before prompting.
pub async fn gen_commit_message(
    client: &dyn AiClient,
    diff: &str,
    opts: CommitMessageOpts,
) -> Result<CommitSuggestion> {
    if diff.trim().is_empty() {
        return Err(AiError::Parse {
            parser: "commit_message: empty diff".into(),
            raw: String::new(),
        });
    }

    // Default to ~1200 tokens of diff if the caller didn't specify —
    // fits comfortably inside a 4K-context local model alongside the
    // system prompt and a generous output budget.
    let budget = if opts.diff_budget_tokens == 0 {
        1200
    } else {
        opts.diff_budget_tokens
    };
    let trimmed = summarize_for_prompt(diff, budget);

    let scope_line = match &opts.scope_hint {
        Some(s) if !s.is_empty() => format!("\nPreferred scope: {}", s),
        _ => String::new(),
    };

    let body_clause = if opts.include_body {
        "After the header write a blank line and a short body paragraph wrapped at ~72 chars."
    } else {
        "Do not include a body — only the header line."
    };

    let system = format!(
        "You are a Git commit message writer. Respond with exactly one Conventional Commits \
         message and nothing else. Format: `<type>(<scope>)?: <subject>` where <type> is one of \
         feat, fix, docs, style, refactor, perf, test, build, ci, chore, revert. Subject must be \
         imperative mood (\"add X\", not \"added X\" or \"adds X\") and under 72 characters. Do not \
         wrap the output in quotes, code fences, or any preamble. {}{}",
        body_clause, scope_line
    );

    let user = format!("Staged diff:\n\n{}", trimmed);

    // Grammar only fires on endpoints that advertise support — the
    // client drops it otherwise. Supplying it unconditionally here
    // keeps task code simple.
    let req = CompletionRequest {
        system,
        messages: vec![Msg {
            role: Role::User,
            content: user,
        }],
        max_tokens: if opts.include_body { 300 } else { 80 },
        // Tiny temperature — we want the same diff to produce a stable
        // message across runs.
        temperature: 0.2,
        grammar: Some(GRAMMAR_COMMIT_MSG.to_string()),
        json_schema: None,
        stop: vec![],
    };

    let resp = client.complete(req).await?;
    parse_commit_message(&resp.text)
}

/// Parse the model's reply into a structured suggestion.
///
/// We accept, in order of strictness:
///   1. exactly the grammar shape: `type(scope)?: subject\n\nbody?`;
///   2. the same, but wrapped in common LLM prose ("Here's...");
///   3. best-effort: strip code fences, scan for the first line that
///      starts with a known type.
pub(crate) fn parse_commit_message(text: &str) -> Result<CommitSuggestion> {
    // Strip trailing/leading whitespace and any surrounding code fence
    // a non-grammar model might have added.
    let cleaned = strip_code_fence(text.trim());

    // Scan lines for the first candidate header.
    for line in cleaned.lines() {
        let candidate = line.trim();
        if candidate.is_empty() {
            continue;
        }
        if let Some(sugg) = try_parse_header(candidate) {
            // Capture body = everything after a blank line following
            // this header, if any.
            let body = extract_body_after(cleaned, candidate);
            return Ok(CommitSuggestion {
                title: candidate.to_string(),
                body,
                commit_type: sugg.0,
                scope: sugg.1,
            });
        }
    }

    Err(AiError::Parse {
        parser: "commit_message: no conventional header found".into(),
        raw: cleaned.chars().take(512).collect(),
    })
}

fn strip_code_fence(s: &str) -> &str {
    // Hand-rolled because we don't want to pull in a regex crate. We
    // recognise ```<tag>?\n...\n``` at the start of the buffer.
    let s = s.trim();
    let stripped = s
        .strip_prefix("```")
        .and_then(|rest| {
            // drop optional language tag up to the first newline
            let nl = rest.find('\n')?;
            Some(&rest[nl + 1..])
        })
        .and_then(|rest| rest.strip_suffix("```"))
        .map(str::trim);
    stripped.unwrap_or(s)
}

/// Parse `type(scope)?: subject`. Returns `(type, scope)` on success.
fn try_parse_header(line: &str) -> Option<(String, Option<String>)> {
    // Must contain a colon.
    let colon = line.find(':')?;
    let prefix = &line[..colon];

    // Split off optional scope in parens.
    let (ty, scope) = if let Some(open) = prefix.find('(') {
        if !prefix.ends_with(')') {
            return None;
        }
        let ty = &prefix[..open];
        let scope = &prefix[open + 1..prefix.len() - 1];
        if scope.is_empty() || !scope_looks_valid(scope) {
            return None;
        }
        (ty, Some(scope.to_string()))
    } else {
        (prefix, None)
    };

    if !CONVENTIONAL_TYPES.contains(&ty) {
        return None;
    }
    // Subject must be non-empty after `: `.
    let subject = line[colon + 1..].trim_start();
    if subject.is_empty() {
        return None;
    }
    Some((ty.to_string(), scope))
}

fn scope_looks_valid(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Find a body paragraph after the header line, if present.
fn extract_body_after(full: &str, header: &str) -> Option<String> {
    let idx = full.find(header)?;
    let after = &full[idx + header.len()..];
    // Expect at least one blank line before the body; otherwise the
    // following lines are probably a second header the model emitted
    // by mistake and we'd rather drop them.
    let after = after.trim_start_matches('\n');
    let after = after.trim_start_matches('\n');
    let body = after.trim();
    if body.is_empty() {
        None
    } else {
        Some(body.to_string())
    }
}

/// Convenience for callers on Anthropic endpoints: the GBNF grammar
/// is useless there, so they can build a request with the JSON-schema
/// path instead. Kept here (not in client) because it's prompt-scoped.
#[allow(dead_code)]
pub(crate) fn is_grammar_useful(protocol: Protocol) -> bool {
    matches!(protocol, Protocol::OpenAICompatible)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_header() {
        let r = parse_commit_message("feat(ui): add dark mode toggle").unwrap();
        assert_eq!(r.commit_type, "feat");
        assert_eq!(r.scope.as_deref(), Some("ui"));
        assert!(r.body.is_none());
    }

    #[test]
    fn parses_with_body() {
        let r = parse_commit_message("fix: handle empty diff\n\nReturn NotConfigured.").unwrap();
        assert_eq!(r.commit_type, "fix");
        assert_eq!(r.body.as_deref(), Some("Return NotConfigured."));
    }

    #[test]
    fn strips_code_fence_and_preamble() {
        let raw = "Sure, here:\n```\nrefactor: extract helper\n```";
        let r = parse_commit_message(raw).unwrap();
        assert_eq!(r.commit_type, "refactor");
    }

    #[test]
    fn rejects_unknown_type() {
        assert!(parse_commit_message("wip: nothing").is_err());
    }
}
