//! Extract this repository's commit-message conventions from `git log`
//! so the AI prompt can anchor on the project's own dialect instead of
//! some generic Conventional Commits template.
//!
//! Motivation
//! ----------
//! "Conventional Commits" is a loose family — every project lands on
//! slightly different conventions: which scope tokens are in rotation
//! (`git`, `ui`, `clone`, …), whether subjects are imperative, whether
//! bodies are expected, how references to issues appear. A model that
//! sees only the diff has to guess, and guesses wrong as often as not.
//! A handful of past commit headers is usually enough to fix the
//! dialect without fine-tuning.
//!
//! Scope of this module
//! --------------------
//! * Read-only: we shell out to `git log` and parse. We never write.
//! * Best-effort: a repo with empty history or malformed headers
//!   returns `RepoConventions::default()` — the caller's prompt then
//!   omits the `PROJECT CONVENTIONS` block entirely, which is fine.
//! * Cached per-path for the process lifetime: reading 100 headers
//!   takes ~20ms on a warm FS but we'd otherwise re-run it on every
//!   commit-message click.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Parsed summary of recent commit behaviour in this repo.
#[derive(Debug, Clone, Default)]
pub struct RepoConventions {
    /// Scope tokens seen in recent headers, ordered by descending
    /// frequency. Caller uses this to validate / bias the model's
    /// scope choice.
    pub common_scopes: Vec<ScopeStat>,
    /// Up to 8 recent header lines verbatim — the cheapest possible
    /// "few-shot examples" for the model.
    pub example_headers: Vec<String>,
    /// Majority subject verb tense seen in recent commits.
    pub subject_style: SubjectStyle,
    /// Fraction of recent commits that included a body (0.0–1.0).
    /// ≥0.5 → the project writes bodies; the caller should set
    /// `include_body = true` by default.
    pub body_rate: f32,
    /// Total commits sampled. `0` means "no reliable signal".
    pub sample_size: usize,
}

#[derive(Debug, Clone)]
pub struct ScopeStat {
    pub scope: String,
    pub count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubjectStyle {
    /// Unknown / not enough signal — model falls back to generic
    /// imperative guidance from the system prompt.
    #[default]
    Unknown,
    /// "add X", "fix Y" — the most common style and the one the
    /// Conventional Commits spec recommends.
    Imperative,
    /// "added X", "fixes Y" — present in some projects; we shouldn't
    /// force imperative on a repo that clearly uses past tense.
    NonImperative,
}

impl RepoConventions {
    /// True when we have enough signal to paste into the prompt.
    /// Below this threshold the caller should omit the block entirely
    /// to avoid biasing the model on noise.
    pub fn is_reliable(&self) -> bool {
        self.sample_size >= 5
    }

    /// Render as a `PROJECT CONVENTIONS` prompt block. Empty string
    /// when `is_reliable` is false — safe to concatenate unconditionally.
    pub fn render_for_prompt(&self) -> String {
        if !self.is_reliable() {
            return String::new();
        }
        use std::fmt::Write;
        let mut out = String::new();
        out.push_str("PROJECT CONVENTIONS (learned from recent commits):\n");

        if !self.common_scopes.is_empty() {
            let sample: Vec<String> = self
                .common_scopes
                .iter()
                .take(10)
                .map(|s| format!("{} ({}×)", s.scope, s.count))
                .collect();
            let _ = writeln!(out, "- Scopes in active use: {}", sample.join(", "));
            let _ = writeln!(
                out,
                "  Prefer one of these over inventing a new scope; introduce a new scope \
                 only when none fits."
            );
        }
        match self.subject_style {
            SubjectStyle::Imperative => {
                let _ = writeln!(out, "- Subject style: imperative (\"add X\", \"fix Y\")");
            }
            SubjectStyle::NonImperative => {
                let _ = writeln!(
                    out,
                    "- Subject style: past/third-person (match the existing dialect; do NOT \
                     force imperative mood)"
                );
            }
            SubjectStyle::Unknown => {}
        }
        if self.body_rate >= 0.5 {
            let _ = writeln!(
                out,
                "- Bodies are common here ({:.0}% of recent commits) — write one.",
                self.body_rate * 100.0
            );
        } else if self.body_rate <= 0.1 {
            let _ = writeln!(
                out,
                "- Bodies are rare here ({:.0}% of recent commits) — header-only is fine.",
                self.body_rate * 100.0
            );
        }
        if !self.example_headers.is_empty() {
            out.push_str("- Recent headers for reference:\n");
            for h in self.example_headers.iter().take(6) {
                let _ = writeln!(out, "    {h}");
            }
        }
        out
    }
}

// ============================================================
// Cache
// ============================================================

/// Process-wide cache: canonical repo path → parsed conventions.
/// `Mutex<HashMap>` is fine — lookups are few per second and miss cost
/// is ~20ms of git I/O, so lock contention is a non-issue.
static CACHE: OnceLock<Mutex<HashMap<PathBuf, RepoConventions>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<PathBuf, RepoConventions>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Load conventions for `repo_path`, using a cached result when
/// available. Forces a refresh is never needed at runtime — the
/// conventions change glacially (new scope appears every few dozen
/// commits) and we're fine showing a slightly stale snapshot.
pub fn load(repo_path: &Path) -> RepoConventions {
    let key = repo_path
        .canonicalize()
        .unwrap_or_else(|_| repo_path.to_path_buf());
    if let Some(hit) = cache().lock().ok().and_then(|g| g.get(&key).cloned()) {
        return hit;
    }
    let computed = compute(&key);
    if let Ok(mut g) = cache().lock() {
        g.insert(key, computed.clone());
    }
    computed
}

// ============================================================
// Core computation
// ============================================================

fn compute(repo_path: &Path) -> RepoConventions {
    let raw = match run_git_log(repo_path) {
        Some(s) => s,
        None => return RepoConventions::default(),
    };

    let commits = split_commits(&raw);
    if commits.is_empty() {
        return RepoConventions::default();
    }

    // Per-commit: parse subject into (type, scope, subject_text) and
    // record whether a body existed. The wire format we asked for from
    // `git log` separates records with a specific marker (see
    // `run_git_log`) so parsing is straightforward.
    let mut scope_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut example_headers = Vec::new();
    let mut imperative_votes = 0usize;
    let mut non_imperative_votes = 0usize;
    let mut with_body = 0usize;

    for commit in &commits {
        let Some(subject_line) = commit.subject.lines().next() else {
            continue;
        };
        let subject_line = subject_line.trim();
        if subject_line.is_empty() {
            continue;
        }
        if example_headers.len() < 8 {
            example_headers.push(subject_line.to_string());
        }
        if !commit.body.trim().is_empty() {
            with_body += 1;
        }

        if let Some(parsed) = parse_header(subject_line) {
            if let Some(scope) = parsed.scope {
                *scope_counts.entry(scope).or_insert(0) += 1;
            }
            match classify_subject_tense(&parsed.subject) {
                SubjectStyle::Imperative => imperative_votes += 1,
                SubjectStyle::NonImperative => non_imperative_votes += 1,
                SubjectStyle::Unknown => {}
            }
        }
    }

    let mut ranked: Vec<(String, usize)> = scope_counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let common_scopes: Vec<ScopeStat> = ranked
        .into_iter()
        .map(|(scope, count)| ScopeStat { scope, count })
        .collect();

    let subject_style = if imperative_votes + non_imperative_votes < 3 {
        SubjectStyle::Unknown
    } else if imperative_votes >= non_imperative_votes {
        SubjectStyle::Imperative
    } else {
        SubjectStyle::NonImperative
    };

    let body_rate = with_body as f32 / commits.len() as f32;

    RepoConventions {
        common_scopes,
        example_headers,
        subject_style,
        body_rate,
        sample_size: commits.len(),
    }
}

// ============================================================
// git log invocation & parsing
// ============================================================

/// Sentinel chosen for low collision probability with real commit
/// content. We split commits on `\x1e` (ASCII Record Separator) and
/// subject/body on `\x1f` (Unit Separator) — same trick `git` itself
/// uses for `--batch-check`.
const COMMIT_SEP: &str = "\x1e";
const FIELD_SEP: &str = "\x1f";

fn run_git_log(repo_path: &Path) -> Option<String> {
    let fmt = format!("%s{FIELD_SEP}%b{COMMIT_SEP}");
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("log")
        .arg("-n")
        .arg("100")
        .arg(format!("--pretty=format:{fmt}"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Debug)]
struct RawCommit {
    subject: String,
    body: String,
}

fn split_commits(raw: &str) -> Vec<RawCommit> {
    raw.split(COMMIT_SEP)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|rec| {
            let (subject, body) = rec.split_once(FIELD_SEP)?;
            Some(RawCommit {
                subject: subject.to_string(),
                body: body.to_string(),
            })
        })
        .collect()
}

struct ParsedHeader {
    #[allow(dead_code)]
    commit_type: String,
    scope: Option<String>,
    subject: String,
}

fn parse_header(line: &str) -> Option<ParsedHeader> {
    let colon = line.find(':')?;
    let prefix = &line[..colon];
    let subject = line[colon + 1..].trim().to_string();
    if subject.is_empty() {
        return None;
    }
    let (ty, scope) = if let Some(open) = prefix.find('(') {
        if !prefix.ends_with(')') {
            return None;
        }
        let ty = prefix[..open].to_string();
        let scope_raw = prefix[open + 1..prefix.len() - 1].trim();
        if scope_raw.is_empty() {
            return None;
        }
        (ty, Some(scope_raw.to_string()))
    } else {
        (prefix.to_string(), None)
    };
    // Drop the `!` breaking-change marker some projects use on the
    // type (`feat!:`) before registering it as a "type".
    let ty = ty.trim_end_matches('!').to_string();
    if ty.is_empty() || !ty.chars().all(|c| c.is_ascii_alphabetic() || c == '-') {
        return None;
    }
    Some(ParsedHeader {
        commit_type: ty,
        scope,
        subject,
    })
}

/// Rough tense classification of a subject string. Not linguistically
/// accurate — we just look at the first word's suffix, which catches
/// >90% of English commit headers without bringing in an NLP stack.
fn classify_subject_tense(subject: &str) -> SubjectStyle {
    let first = subject.split_whitespace().next().unwrap_or("");
    let lower = first.to_ascii_lowercase();
    if lower.is_empty() {
        return SubjectStyle::Unknown;
    }
    // `-ed` / `-s` suffixes on the first verb → non-imperative.
    // Exceptions: short function-y words like "adds" don't count if
    // the word is also a plausible imperative (`cross`, `focus`, …);
    // we accept the false-positive-for-imperative tradeoff.
    if lower.ends_with("ed") && lower.len() > 3 {
        return SubjectStyle::NonImperative;
    }
    if lower.ends_with('s') && lower.len() > 3 && !lower.ends_with("ss") {
        return SubjectStyle::NonImperative;
    }
    SubjectStyle::Imperative
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_header_extracts_scope_and_subject() {
        let p = parse_header("feat(clone): add probe").unwrap();
        assert_eq!(p.commit_type, "feat");
        assert_eq!(p.scope.as_deref(), Some("clone"));
        assert_eq!(p.subject, "add probe");
    }

    #[test]
    fn parse_header_allows_breaking_marker() {
        let p = parse_header("feat!: drop legacy api").unwrap();
        assert_eq!(p.commit_type, "feat");
        assert!(p.scope.is_none());
    }

    #[test]
    fn parse_header_rejects_garbage() {
        assert!(parse_header("no colon here").is_none());
        assert!(parse_header("12345(foo): subject").is_none());
    }

    #[test]
    fn classify_tense_imperative() {
        assert_eq!(
            classify_subject_tense("add dark mode"),
            SubjectStyle::Imperative
        );
        assert_eq!(
            classify_subject_tense("fix race condition"),
            SubjectStyle::Imperative
        );
    }

    #[test]
    fn classify_tense_non_imperative() {
        assert_eq!(
            classify_subject_tense("added dark mode"),
            SubjectStyle::NonImperative
        );
        assert_eq!(
            classify_subject_tense("fixes race condition"),
            SubjectStyle::NonImperative
        );
    }

    #[test]
    fn split_commits_parses_record_format() {
        let raw = format!(
            "feat(a): one{FIELD_SEP}first body{COMMIT_SEP}fix(b): two{FIELD_SEP}{COMMIT_SEP}"
        );
        let commits = split_commits(&raw);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].subject, "feat(a): one");
        assert_eq!(commits[0].body, "first body");
        assert_eq!(commits[1].body, "");
    }

    #[test]
    fn prompt_block_empty_when_unreliable() {
        let c = RepoConventions::default();
        assert!(c.render_for_prompt().is_empty());
    }

    #[test]
    fn prompt_block_lists_scopes_and_style() {
        let c = RepoConventions {
            common_scopes: vec![
                ScopeStat {
                    scope: "git".into(),
                    count: 8,
                },
                ScopeStat {
                    scope: "ui".into(),
                    count: 5,
                },
            ],
            example_headers: vec!["feat(git): add rebase".into()],
            subject_style: SubjectStyle::Imperative,
            body_rate: 0.7,
            sample_size: 15,
        };
        let s = c.render_for_prompt();
        assert!(s.contains("git (8×)"));
        assert!(s.contains("imperative"));
        assert!(s.contains("Bodies are common"));
        assert!(s.contains("feat(git): add rebase"));
    }
}
