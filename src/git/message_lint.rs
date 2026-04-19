//! Commit-message linter: a small rule engine driven by
//! `.mergefox/rules.toml` at the repo root.
//!
//! Motivation
//! ----------
//! Teams repeatedly want to block or flag specific phrases in commit
//! messages — competitor brand names, `wip` / `asdf` placeholders,
//! credentials that shouldn't leak into history. A plain git commit-msg
//! hook works for the CLI but most non-power-users never install one.
//! Baking the linter into the commit modal means every contributor who
//! opens the repo in mergeFox gets the same preflight, with zero setup.
//!
//! Rule format
//! -----------
//! The config file is TOML with an array of `[[rule]]` tables:
//!
//!   [[rule]]
//!   pattern = "Fork-style"
//!   replacement = "our-style"   # optional — drives quick-fix
//!   severity = "error"           # "error" | "warn" (default "warn")
//!   reason = "Avoid competitor brand names in commit messages."
//!   scope = "message"            # "subject" | "body" | "message" (default "message")
//!
//! A single file can have any number of rules; they're all checked on
//! every commit.
//!
//! Matching is literal string `contains` — deliberately simple so that
//! non-technical teams can edit the file. Regex support is a natural
//! follow-up but adds a meaningful review surface (injection, ReDoS)
//! we don't need in v1.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Severity of a rule. `Error` blocks the commit from the modal's
/// primary button; `Warn` shows a banner but lets the user proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warn,
}

impl Default for Severity {
    fn default() -> Self {
        Severity::Warn
    }
}

/// Which slice of the message a rule applies to. Keeping it explicit
/// lets a team restrict a rule to the subject line ("no emoji in
/// subject") without false-positives on prose in the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Subject,
    Body,
    Message,
}

impl Default for Scope {
    fn default() -> Self {
        Scope::Message
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub pattern: String,
    #[serde(default)]
    pub replacement: Option<String>,
    #[serde(default)]
    pub severity: Severity,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub scope: Scope,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RulesFile {
    #[serde(default)]
    pub rule: Vec<Rule>,
}

/// A single lint finding, produced by applying a rule to a message.
#[derive(Debug, Clone)]
pub struct Finding {
    pub pattern: String,
    pub replacement: Option<String>,
    pub severity: Severity,
    pub reason: Option<String>,
    pub scope: Scope,
    /// Byte offset of the first match within the original message.
    /// UI surfaces it for "click to jump" later; unused in v1 but kept
    /// so callers don't need an API bump when it lands.
    pub match_offset: usize,
}

impl Finding {
    pub fn is_error(&self) -> bool {
        matches!(self.severity, Severity::Error)
    }
}

/// Load the rules file at `<repo>/.mergefox/rules.toml`. Returns an
/// empty ruleset if the file doesn't exist — absence is the normal
/// "no lint configured" case, not an error.
pub fn load_rules(repo_path: &Path) -> Result<RulesFile> {
    let path = rules_file_path(repo_path);
    if !path.exists() {
        return Ok(RulesFile::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let parsed: RulesFile = toml::from_str(&text)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(parsed)
}

pub fn rules_file_path(repo_path: &Path) -> PathBuf {
    repo_path.join(".mergefox").join("rules.toml")
}

/// Run every rule against `message` and return the findings.
///
/// The message is split into subject (the part before the first blank
/// line, or the whole string) and body (the remainder) so rules scoped
/// to a particular section behave as documented. The literal
/// `contains` check is case-sensitive on purpose: banned words are
/// usually brand names, and case-insensitive matching would flag legit
/// prose like "fork the repository" when the rule means "Fork (the
/// product)".
pub fn lint(message: &str, rules: &RulesFile) -> Vec<Finding> {
    let mut out = Vec::new();
    if rules.rule.is_empty() || message.is_empty() {
        return out;
    }
    let (subject, body_offset) = split_subject(message);
    let body = &message[body_offset..];

    for rule in &rules.rule {
        if rule.pattern.is_empty() {
            continue;
        }
        let (slice, base_offset) = match rule.scope {
            Scope::Subject => (subject, 0usize),
            Scope::Body => (body, body_offset),
            Scope::Message => (message, 0usize),
        };
        if let Some(idx) = slice.find(&rule.pattern) {
            out.push(Finding {
                pattern: rule.pattern.clone(),
                replacement: rule.replacement.clone(),
                severity: rule.severity,
                reason: rule.reason.clone(),
                scope: rule.scope,
                match_offset: base_offset + idx,
            });
        }
    }
    out
}

/// Apply every finding's `replacement` to `message` (when set). Used
/// by the commit-modal's quick-fix button. Findings without a
/// replacement are left alone.
pub fn auto_fix(message: &str, findings: &[Finding]) -> String {
    let mut out = message.to_string();
    for f in findings {
        if let Some(rep) = &f.replacement {
            out = out.replace(&f.pattern, rep);
        }
    }
    out
}

fn split_subject(message: &str) -> (&str, usize) {
    // Conventional-commit convention: subject is the first line (or
    // everything up to the first blank line). If the message has no
    // blank line, the entire string is the subject.
    let mut byte_idx = 0;
    for line in message.split_inclusive('\n') {
        if line.trim().is_empty() && byte_idx > 0 {
            return (&message[..byte_idx], byte_idx + line.len());
        }
        byte_idx += line.len();
    }
    (message, message.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules_from(toml_str: &str) -> RulesFile {
        toml::from_str(toml_str).unwrap()
    }

    #[test]
    fn lint_flags_substring_in_subject() {
        let rules = rules_from(
            r#"
            [[rule]]
            pattern = "Fork-style"
            severity = "error"
            reason = "competitor brand"
            "#,
        );
        let findings = lint("Fork-style tabs look great\n\nbody", &rules);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].is_error());
        assert_eq!(findings[0].pattern, "Fork-style");
    }

    #[test]
    fn lint_scope_subject_ignores_body_hit() {
        let rules = rules_from(
            r#"
            [[rule]]
            pattern = "wip"
            severity = "warn"
            scope = "subject"
            "#,
        );
        let findings = lint("feat: real subject\n\nsome wip thing", &rules);
        assert!(findings.is_empty());
    }

    #[test]
    fn lint_scope_body_ignores_subject_hit() {
        let rules = rules_from(
            r#"
            [[rule]]
            pattern = "TODO"
            severity = "warn"
            scope = "body"
            "#,
        );
        let findings = lint("TODO: ship\n\nno body hit", &rules);
        assert!(findings.is_empty());
    }

    #[test]
    fn auto_fix_applies_replacement() {
        let rules = rules_from(
            r#"
            [[rule]]
            pattern = "Fork-style"
            replacement = "our-style"
            severity = "warn"
            "#,
        );
        let findings = lint("Fork-style\n\nmore Fork-style", &rules);
        let fixed = auto_fix("Fork-style\n\nmore Fork-style", &findings);
        assert_eq!(fixed, "our-style\n\nmore our-style");
    }

    #[test]
    fn lint_without_rules_returns_nothing() {
        let rules = RulesFile::default();
        let findings = lint("anything", &rules);
        assert!(findings.is_empty());
    }

    #[test]
    fn empty_pattern_is_ignored() {
        let rules = rules_from(
            r#"
            [[rule]]
            pattern = ""
            severity = "error"
            "#,
        );
        let findings = lint("nothing to see", &rules);
        assert!(findings.is_empty());
    }
}
