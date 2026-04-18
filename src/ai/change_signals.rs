//! Pre-LLM classification of what a diff actually represents.
//!
//! The core problem we're solving: a language model handed a raw unified
//! diff consistently mis-classifies commits as `docs` whenever the
//! surface area of added `///` comments happens to exceed functional
//! code — even though a brand-new module with 200 lines of implementation
//! is clearly a feature. Line counts are the wrong signal.
//!
//! Instead, we compute a structured snapshot of the change's *shape*
//! — new public API? behaviour modification? tests only? — and feed
//! those booleans and lists to the model as a `CLASSIFICATION SIGNALS`
//! block. The model then conditions on semantic facts it couldn't
//! reliably extract from the hunk stream by itself.
//!
//! Deliberate non-goals
//! --------------------
//! * No syntax tree: we work line-by-line with conservative heuristics.
//!   `syn` would be more accurate for Rust but useless for the other
//!   languages a polyglot user can commit. The false-positive rate is
//!   acceptable because the LLM treats these as hints, not ground truth.
//! * No model call: every signal here is deterministic preprocessing so
//!   the classification is stable, debuggable, and adds zero latency.
//! * No line-count weighting: we explicitly reject "volume equals
//!   significance" because a one-line flip of a feature flag is often
//!   more semantically significant than a 500-line rename pass.

use std::collections::{BTreeMap, BTreeSet};

/// Everything we know about a single file touched by the diff.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: String,
    /// True if this file didn't exist before the diff (`new file mode`).
    pub is_new: bool,
    /// True if the diff deletes the file entirely (`deleted file mode`).
    pub is_deleted: bool,
    /// True if the path sits under a test directory or uses a test-file
    /// naming convention.
    pub is_test_path: bool,
    /// True if the path is a documentation file (Markdown, plain text,
    /// RST, changelogs, license files).
    pub is_doc_path: bool,
    /// Added `+` lines, content only (no leading `+`).
    pub added: Vec<String>,
    /// Removed `-` lines, content only.
    pub removed: Vec<String>,
}

/// Complete semantic summary of a diff, ready to be rendered into the
/// model prompt. Every field is either a boolean fact the model can
/// condition on directly, or a small list it can borrow verbatim.
#[derive(Debug, Clone, Default)]
pub struct ChangeSignals {
    /// Signal 1 — at least one new public item was added.
    pub new_public_api: bool,
    /// Concrete public symbols introduced (max 8 shown in the prompt).
    pub new_public_symbols: Vec<String>,

    /// Signal 2 — control flow, matching, or side-effect calls were
    /// added on top of the prior version. Modifies, not just reflows.
    pub behavior_change: bool,

    /// Signal 3 — the diff looks like a bug fix: issue / fix / error /
    /// panic keywords in added content, or new error-handling code.
    pub bug_error_context: bool,

    /// Signal 4 — every added line is a comment, doc comment, or sits
    /// in a documentation file. True here means `docs` is the honest
    /// type; false means `docs` is forbidden.
    pub docs_only: bool,

    /// Signal 5 — every modified path is under a test directory / uses
    /// a test naming convention.
    pub tests_only: bool,

    /// Signal 6 — looks like a rename or pure reorganisation: balanced
    /// +/- counts, no new public API, not docs-only, not tests-only.
    pub refactor_like: bool,

    /// Signal 7 — the first sentence of the top-level doc comment on a
    /// freshly added module or file, usable as a subject seed.
    pub intent_hint: Option<String>,

    /// Most-affected top-level module (e.g. "clone", "ui", "ai").
    pub dominant_module: Option<String>,

    /// All touched files — exposed so the prompt renderer can list them.
    pub files: Vec<FileChange>,

    /// Signal 8 — when the diff spans multiple independent concerns,
    /// the caller should prefer `commit_composer` (multi-commit split)
    /// over a single commit message. `None` means "looks like a single
    /// coherent commit".
    pub segmentation_advice: Option<SegmentationAdvice>,
}

/// Why the analyser thinks this diff shouldn't live in a single commit,
/// along with the proposed partitioning so the UI can drive the user
/// to the composer flow or at least pre-stage one group at a time.
#[derive(Debug, Clone)]
pub struct SegmentationAdvice {
    pub reason: String,
    pub groups: Vec<SegmentationGroup>,
}

#[derive(Debug, Clone)]
pub struct SegmentationGroup {
    /// Short tag describing the group — usually a module name or
    /// "docs" / "tests" — usable as a scope candidate.
    pub label: String,
    pub paths: Vec<String>,
}

impl ChangeSignals {
    /// A starter type guess based purely on signals. The LLM may
    /// override it; we include it in the prompt as `suggested_type`
    /// alongside the raw signals so the model can reason over both.
    pub fn suggested_type(&self) -> &'static str {
        if self.tests_only {
            return "test";
        }
        if self.docs_only {
            return "docs";
        }
        if self.bug_error_context && !self.new_public_api {
            return "fix";
        }
        if self.refactor_like {
            return "refactor";
        }
        if self.new_public_api || self.behavior_change {
            return "feat";
        }
        // Everything that isn't clearly any of the above — default to
        // feat; the LLM still sees the raw signals and can pick better.
        "feat"
    }

    /// Render as a prompt-ready block. Designed to be pasted directly
    /// above the trimmed diff so the model sees the signals first.
    pub fn render_for_prompt(&self) -> String {
        use std::fmt::Write;

        let mut out = String::new();
        out.push_str("CLASSIFICATION SIGNALS:\n");

        // Bool signals — render as YES/NO so the model doesn't miss a
        // missing boolean (language models are better at reading
        // explicit tokens than absences).
        let _ = writeln!(
            out,
            "- New public API surface: {} ({} new symbol{})",
            yn(self.new_public_api),
            self.new_public_symbols.len(),
            if self.new_public_symbols.len() == 1 { "" } else { "s" }
        );
        if !self.new_public_symbols.is_empty() {
            let sample: Vec<&str> = self
                .new_public_symbols
                .iter()
                .take(8)
                .map(String::as_str)
                .collect();
            let _ = writeln!(out, "  Examples: {}", sample.join(", "));
        }
        let _ = writeln!(out, "- Behavior change: {}", yn(self.behavior_change));
        let _ = writeln!(
            out,
            "- Bug/error context (fix-like): {}",
            yn(self.bug_error_context)
        );
        let _ = writeln!(
            out,
            "- Documentation/comments only: {}",
            yn(self.docs_only)
        );
        let _ = writeln!(out, "- Test-only change: {}", yn(self.tests_only));
        let _ = writeln!(out, "- Refactor-like (rename/reorganise): {}", yn(self.refactor_like));

        if let Some(module) = &self.dominant_module {
            let _ = writeln!(out, "- Dominant module: {module}");
        }
        if let Some(hint) = &self.intent_hint {
            let _ = writeln!(out, "- Intent hint (from new module doc): {hint}");
        }

        let paths: Vec<&str> = self
            .files
            .iter()
            .take(10)
            .map(|f| f.path.as_str())
            .collect();
        let _ = writeln!(
            out,
            "- Touched files ({}): {}{}",
            self.files.len(),
            paths.join(", "),
            if self.files.len() > paths.len() { ", …" } else { "" }
        );

        let _ = writeln!(out, "- Suggested type: {}", self.suggested_type());

        if let Some(advice) = &self.segmentation_advice {
            let _ = writeln!(
                out,
                "- WARNING — multi-concern diff: {}",
                advice.reason
            );
            for g in &advice.groups {
                let sample: Vec<&str> =
                    g.paths.iter().take(4).map(String::as_str).collect();
                let more = if g.paths.len() > sample.len() {
                    format!(", (+{} more)", g.paths.len() - sample.len())
                } else {
                    String::new()
                };
                let _ = writeln!(
                    out,
                    "    [{}] {}{}",
                    g.label,
                    sample.join(", "),
                    more
                );
            }
            let _ = writeln!(
                out,
                "    (Write the message for the SINGLE dominant concern; flag the rest as split candidates.)"
            );
        }

        out
    }
}

fn yn(b: bool) -> &'static str {
    if b { "YES" } else { "NO" }
}

/// Entry point: parse a unified diff and compute all signals.
pub fn analyze(diff: &str) -> ChangeSignals {
    let files = parse_files(diff);
    if files.is_empty() {
        return ChangeSignals::default();
    }

    let new_public_symbols = collect_new_public_symbols(&files);
    let docs_only = compute_docs_only(&files);
    let tests_only = compute_tests_only(&files);
    let behavior_change = compute_behavior_change(&files);
    let bug_error_context = compute_bug_error_context(&files);
    let refactor_like = compute_refactor_like(
        &files,
        !new_public_symbols.is_empty(),
        docs_only,
        tests_only,
        behavior_change,
    );
    let intent_hint = extract_intent_hint(&files);
    let dominant_module = compute_dominant_module(&files);
    let segmentation_advice = compute_segmentation_advice(
        &files,
        &new_public_symbols,
        docs_only,
        tests_only,
    );

    ChangeSignals {
        new_public_api: !new_public_symbols.is_empty(),
        new_public_symbols,
        behavior_change,
        bug_error_context,
        docs_only,
        tests_only,
        refactor_like,
        intent_hint,
        dominant_module,
        files,
        segmentation_advice,
    }
}

// ============================================================
// Diff parsing
// ============================================================

fn parse_files(diff: &str) -> Vec<FileChange> {
    let mut files = Vec::new();
    let mut cur: Option<FileChange> = None;
    let mut in_hunk = false;

    for line in diff.lines() {
        if let Some(path) = parse_git_header(line) {
            if let Some(f) = cur.take() {
                files.push(f);
            }
            cur = Some(FileChange {
                path: path.to_string(),
                is_new: false,
                is_deleted: false,
                is_test_path: is_test_path(path),
                is_doc_path: is_doc_path(path),
                added: Vec::new(),
                removed: Vec::new(),
            });
            in_hunk = false;
            continue;
        }
        let Some(f) = cur.as_mut() else { continue };

        // Meta lines before the first hunk start.
        if line.starts_with("new file mode") {
            f.is_new = true;
            continue;
        }
        if line.starts_with("deleted file mode") {
            f.is_deleted = true;
            continue;
        }
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }

        // Ignore the `+++ b/path` / `--- a/path` header lines that live
        // inside each file block before the first hunk — they were
        // filtered above by the hunk guard, but if the diff is unusual
        // we double-check here.
        if line.starts_with("+++ ") || line.starts_with("--- ") {
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            f.added.push(rest.to_string());
        } else if let Some(rest) = line.strip_prefix('-') {
            f.removed.push(rest.to_string());
        }
    }
    if let Some(f) = cur.take() {
        files.push(f);
    }
    files
}

fn parse_git_header(line: &str) -> Option<&str> {
    // `diff --git a/path b/path` — we take the `a/` side. When the
    // file is newly created the `a/` path is still present (points to
    // `/dev/null` conceptually but git still writes `a/path`).
    let rest = line.strip_prefix("diff --git ")?;
    let a_segment = rest.split_whitespace().next()?;
    a_segment.strip_prefix("a/")
}

// ============================================================
// Path classification
// ============================================================

fn is_test_path(path: &str) -> bool {
    // Heuristics that hold across Rust, JS/TS, Python, Go, Java.
    let lower = path.to_ascii_lowercase();
    let segments: Vec<&str> = lower.split('/').collect();

    if segments.iter().any(|s| matches!(*s, "tests" | "test" | "__tests__" | "spec" | "specs")) {
        return true;
    }
    let file = segments.last().copied().unwrap_or("");
    file.ends_with("_test.rs")
        || file.ends_with("_tests.rs")
        || file.ends_with(".test.ts")
        || file.ends_with(".test.tsx")
        || file.ends_with(".test.js")
        || file.ends_with(".spec.ts")
        || file.ends_with(".spec.js")
        || file.ends_with("_test.go")
        || file.ends_with("_test.py")
        || file.starts_with("test_")
}

fn is_doc_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    let file = lower.rsplit('/').next().unwrap_or("");
    if matches!(ext, "md" | "markdown" | "txt" | "rst" | "adoc") {
        return true;
    }
    matches!(
        file,
        "readme" | "changelog" | "notice" | "license" | "license.md" | "authors" | "contributing"
    ) || file.starts_with("readme.")
        || file.starts_with("changelog")
        || file.starts_with("license")
}

// ============================================================
// Signal 1: new public API surface
// ============================================================

/// Yield only the "real code" added lines from a file — drop any lines
/// that sit inside Rust raw string literals or follow a `#[cfg(test)]`
/// / `#[test]` attribute. Shared by every signal computation so none
/// of them independently mis-classify test fixtures or embedded diff
/// literals as real code.
fn code_added_lines(file: &FileChange) -> Vec<&String> {
    let mut kept: Vec<&String> = Vec::new();
    let mut in_raw_string: Option<String> = None;
    let mut in_cfg_test = false;
    for line in &file.added {
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[cfg(test)]") || trimmed.starts_with("#[test]") {
            in_cfg_test = true;
            continue;
        }
        if let Some(closer) = in_raw_string.clone() {
            if line.contains(&closer) {
                in_raw_string = None;
            }
            continue;
        }
        if let Some(closer) = detect_raw_string_opener(line) {
            if !line.contains(&closer) || line.matches(&closer).count() < 2 {
                in_raw_string = Some(closer);
            }
            continue;
        }
        if in_cfg_test {
            continue;
        }
        kept.push(line);
    }
    kept
}

/// Match `pub fn|struct|enum|trait|mod|const|static|type`. We ignore
/// `pub use` renames on purpose — re-exporting an existing symbol is
/// not the same as adding new public surface, and treating it as
/// "feat" would misclassify routine module cleanup.
fn collect_new_public_symbols(files: &[FileChange]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for file in files {
        // Paths we won't count as "public API": tests, docs, and
        // build-only manifests — a new `pub fn` inside `#[cfg(test)]`
        // is not user-visible API.
        if file.is_test_path || file.is_doc_path {
            continue;
        }
        for line in code_added_lines(file) {
            if let Some(sym) = extract_pub_symbol(line) {
                let qualified = format!("{}::{sym}", short_module(&file.path));
                if seen.insert(qualified.clone()) {
                    out.push(qualified);
                }
            }
        }
    }
    out
}

/// If `line` contains a Rust raw-string opener (`r"` or `r#...#"`) that
/// isn't yet closed on the same line, return the matching closer so
/// the caller can scan forward for it.
fn detect_raw_string_opener(line: &str) -> Option<String> {
    // Walk the line byte-by-byte. When we see `r` followed by zero or
    // more `#` followed by `"`, we've found an opener. The closer is
    // `"` followed by the same count of `#`.
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'r' {
            let mut j = i + 1;
            let mut hashes = 0;
            while j < bytes.len() && bytes[j] == b'#' {
                hashes += 1;
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'"' {
                let closer = format!("\"{}", "#".repeat(hashes));
                // Is there an earlier closer on the same line? If so,
                // the string is self-contained; skip it.
                let opener_end = j + 1;
                if let Some(rel) = line[opener_end..].find(&closer) {
                    // Move past the self-contained string and keep
                    // scanning for a later, longer opener.
                    i = opener_end + rel + closer.len();
                    continue;
                }
                return Some(closer);
            }
        }
        i += 1;
    }
    None
}

fn extract_pub_symbol(raw: &str) -> Option<String> {
    let line = raw.trim_start();
    // `pub(crate)` / `pub(super)` also count — they expand public
    // surface within the crate. Strip the visibility qualifier.
    let rest = strip_pub_prefix(line)?;
    // Kinds that introduce a named item. Excludes `use`.
    for kw in ["fn ", "struct ", "enum ", "trait ", "mod ", "const ", "static ", "type "] {
        if let Some(after) = rest.strip_prefix(kw) {
            let name: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

fn strip_pub_prefix(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("pub")?;
    // Either `pub ` (space) or `pub(...)` (crate/super visibility).
    if let Some(after_space) = rest.strip_prefix(' ') {
        return Some(after_space.trim_start());
    }
    if rest.starts_with('(') {
        let close = rest.find(')')?;
        let after = &rest[close + 1..];
        return Some(after.trim_start());
    }
    None
}

fn short_module(path: &str) -> String {
    // `src/clone_auth.rs`           → `clone_auth`
    // `src/ui/welcome.rs`           → `welcome`
    // `src/ai/mod.rs` / `lib.rs`    → `ai`  (parent dir, because the
    //                                        file name carries no name)
    let segments: Vec<&str> = path.split('/').collect();
    let file = *segments.last().unwrap_or(&path);
    let stem = file
        .strip_suffix(".rs")
        .or_else(|| file.strip_suffix(".ts"))
        .or_else(|| file.strip_suffix(".tsx"))
        .or_else(|| file.strip_suffix(".js"))
        .or_else(|| file.strip_suffix(".py"))
        .or_else(|| file.strip_suffix(".go"))
        .unwrap_or(file);
    // Only `mod.rs` / `lib.rs` derive their name from the parent dir —
    // those are "module root" files with no name of their own. `main`
    // and `index` ARE meaningful scope names in their own right
    // (`src/main.rs` belongs in scope `main`, not `src`), so we keep
    // them verbatim.
    if matches!(stem, "mod" | "lib") && segments.len() >= 2 {
        return segments[segments.len() - 2].to_string();
    }
    stem.to_string()
}

// ============================================================
// Signal 2: behavior change
// ============================================================

/// A line that moves the machine — control flow, iteration, match,
/// side-effect calls, panics. Comments and pure data definitions do
/// not count. Used both for "behaviour changed" detection and as a
/// tie-breaker against `docs_only`.
fn line_is_behavioural(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    if is_comment_line(trimmed) {
        return false;
    }

    // Rust / general control flow tokens.
    const FLOW_TOKENS: &[&str] = &[
        "if ", "else", "match ", "for ", "while ", "loop", "return",
        "break", "continue", "await", "?", "yield", "throw",
    ];
    if FLOW_TOKENS.iter().any(|tok| trimmed.contains(tok)) {
        return true;
    }

    // Side-effect / error-path calls. Conservative list — we're happy
    // to miss some; a false negative here just fails to flip the
    // signal, it doesn't corrupt the prompt.
    const SIDE_EFFECT: &[&str] = &[
        "panic!", "unreachable!", "todo!", "unimplemented!",
        "bail!", "ensure!", ".expect(", ".unwrap(", ".unwrap_or",
        ".map_err", ".ok_or", "Err(", "Some(", "None",
        "Command::", "spawn(", "send(", "write!", "writeln!", "println!",
        "fs::", "std::thread", "std::sync",
    ];
    if SIDE_EFFECT.iter().any(|tok| trimmed.contains(tok)) {
        return true;
    }

    // Assignment to a mutable binding or field update — `let mut x =`,
    // `x = ...;`, `self.x = ...;`. This catches behaviour added via
    // state mutation that isn't captured by the lists above.
    if trimmed.starts_with("let mut ") {
        return true;
    }
    if trimmed.contains(" = ") && !trimmed.contains(" == ") && !trimmed.starts_with("//") {
        // Rough: looks like an assignment statement (not an equality
        // check). False positives on struct-init lines are fine — a
        // struct init inside a new function is itself behaviour.
        return true;
    }

    false
}

fn compute_behavior_change(files: &[FileChange]) -> bool {
    // Net behaviour change = behavioural lines added minus behavioural
    // lines removed. A rename / reshuffle preserves the count on both
    // sides (old fn body re-added under a new name) and nets to zero
    // or close to it, so `refactor_like` can still kick in.
    //
    // For newly-created files we do count added behavioural lines as
    // a positive signal regardless, since there's no "before" side —
    // a new module with implementation is unambiguously feature work.
    let mut net: i64 = 0;
    for file in files {
        if file.is_doc_path {
            continue;
        }
        let added = file.added.iter().filter(|l| line_is_behavioural(l)).count() as i64;
        let removed = file.removed.iter().filter(|l| line_is_behavioural(l)).count() as i64;
        if file.is_new {
            net += added;
        } else {
            net += added - removed;
        }
        if net >= 2 {
            return true;
        }
    }
    net >= 2
}

// ============================================================
// Signal 3: bug / error context
// ============================================================

fn compute_bug_error_context(files: &[FileChange]) -> bool {
    // Two distinct hit kinds:
    //   * INLINE FIX ANNOTATIONS — short `// fix: ...` / `// bug: ...`
    //     style comments that devs write alongside a bug fix. A single
    //     one of these is a very strong signal.
    //   * CODE-LEVEL KEYWORDS — keywords in non-comment content like
    //     identifier names or log messages. Weaker signal; we need
    //     multiple hits or corroborating error-path growth.
    const CODE_KEYWORDS: &[&str] = &[
        "fixes #", "fix #", "bugfix", "regression in ", "panicked",
        "off-by-one", "race condition in ", "deadlock in ", "crash when ",
        "segfault", "use-after-free", "double free",
    ];

    let mut inline_annotations = 0usize;
    let mut code_keyword_hits = 0usize;
    for file in files {
        for line in code_added_lines(file) {
            let trimmed = line.trim_start();
            if is_inline_fix_annotation(trimmed) {
                inline_annotations += 1;
                continue;
            }
            // Skip docblock comments so long module preambles
            // mentioning "fix ..." in passing don't count.
            if is_comment_line(trimmed) {
                continue;
            }
            // Strip string-literal content before matching keywords.
            // Without this, the detector's OWN keyword array (e.g.
            // `const CODE_KEYWORDS = &["bugfix", "panicked", …]`) trips
            // the rule and marks a feat as fix-like when all we added
            // was the detector itself. Rare legitimate cases — log
            // messages like `log::error!("fix needed")` — are lower-
            // signal than identifiers / control flow anyway.
            let stripped = strip_string_literals(line);
            let lower = stripped.to_ascii_lowercase();
            if CODE_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
                code_keyword_hits += 1;
            }
        }
    }

    // Inline annotation alone is decisive.
    if inline_annotations >= 1 {
        return true;
    }
    // Two or more code-level hits is also decisive.
    if code_keyword_hits >= 2 {
        return true;
    }

    // Additionally: new `Err(` arms or new `?` propagation where the
    // surrounding function wasn't previously fallible. We can't prove
    // the latter without type info, so we approximate with "non-trivial
    // growth in error construction in an existing file" — new files
    // get excluded because those are typically feat, not fix.
    let mut err_growth = 0i64;
    for file in files {
        if file.is_new {
            continue;
        }
        if file.is_test_path || file.is_doc_path {
            continue;
        }
        let new_err_sites = file
            .added
            .iter()
            .filter(|l| {
                let t = l.trim_start();
                t.starts_with("Err(") || t.starts_with("return Err(") || t.contains("bail!(") || t.contains("anyhow!(")
            })
            .count() as i64;
        let removed_err_sites = file
            .removed
            .iter()
            .filter(|l| {
                let t = l.trim_start();
                t.starts_with("Err(") || t.starts_with("return Err(") || t.contains("bail!(")
            })
            .count() as i64;
        err_growth += new_err_sites - removed_err_sites;
    }
    // Require meaningful net error-path growth (≥3 sites) AND at
    // least one code-level keyword hit before concluding this is a
    // fix. Either alone is too easy to trip on a routine feature
    // that happens to add error handling.
    err_growth >= 3 && code_keyword_hits >= 1
}

/// Replace the content of `""` string literals with equal-length
/// spaces so downstream keyword matching doesn't see the literal
/// text. Handles escape sequences by consuming the next char after a
/// backslash. Raw strings (`r"..."` / `r#"..."#`) are intentionally
/// NOT stripped here — those span multiple lines and are handled by
/// the dedicated state tracker in `collect_new_public_symbols`.
fn strip_string_literals(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_str = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_str {
            if c == '\\' {
                // Consume escape sequence target — the char after the
                // backslash doesn't close the string even if it's `"`.
                let _ = chars.next();
                out.push(' ');
                out.push(' ');
                continue;
            }
            if c == '"' {
                in_str = false;
                out.push('"');
            } else {
                out.push(' ');
            }
        } else {
            if c == '"' {
                in_str = true;
            }
            out.push(c);
        }
    }
    out
}

/// Recognise short inline bug-fix annotations like `// fix: handle ...`
/// or `# bug: off-by-one in loop`. These are deliberate dev shorthand
/// that signals "this hunk is part of a fix" — distinct from longer
/// docstrings that just happen to mention a fix.
fn is_inline_fix_annotation(trimmed: &str) -> bool {
    let stripped = match () {
        _ if trimmed.starts_with("//") => trimmed.trim_start_matches('/'),
        _ if trimmed.starts_with('#') && !trimmed.starts_with("#[") => trimmed.trim_start_matches('#'),
        _ => return false,
    };
    let s = stripped.trim_start().to_ascii_lowercase();
    // Only match the canonical short prefixes to avoid false positives
    // on "this fixes the way we parse X" buried inside paragraphs.
    s.starts_with("fix:")
        || s.starts_with("fix(")
        || s.starts_with("fixes:")
        || s.starts_with("fixes(")
        || s.starts_with("bug:")
        || s.starts_with("bug(")
        || s.starts_with("bugfix:")
        || s.starts_with("hotfix:")
}

// ============================================================
// Signal 4: docs-only
// ============================================================

fn compute_docs_only(files: &[FileChange]) -> bool {
    // Empty diff — nothing added, nothing to classify. Return false so
    // the caller doesn't mislabel a no-op as docs.
    if files.iter().all(|f| f.added.is_empty()) {
        return false;
    }
    for file in files {
        // A file exclusively at a doc path is automatically "doc-only"
        // even if its lines look like code (e.g. markdown code fences).
        if file.is_doc_path {
            continue;
        }
        for line in &file.added {
            let trimmed = line.trim_start();
            if trimmed.is_empty() {
                continue;
            }
            if !is_comment_line(trimmed) {
                return false;
            }
        }
    }
    true
}

fn is_comment_line(trimmed: &str) -> bool {
    // Rust, C/C++/Java/JS/TS, Python, shell/toml, HTML/XML.
    trimmed.starts_with("///")
        || trimmed.starts_with("//!")
        || trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with('#')
        || trimmed.starts_with("<!--")
        || trimmed.starts_with("\"\"\"")
}

// ============================================================
// Signal 5: tests-only
// ============================================================

fn compute_tests_only(files: &[FileChange]) -> bool {
    if files.is_empty() {
        return false;
    }
    files.iter().all(|f| f.is_test_path)
}

// ============================================================
// Signal 6: refactor-like
// ============================================================

fn compute_refactor_like(
    files: &[FileChange],
    has_new_public: bool,
    docs_only: bool,
    tests_only: bool,
    behavior_change: bool,
) -> bool {
    if has_new_public || docs_only || tests_only {
        return false;
    }
    // Heuristic: "balanced +/-" — roughly as many removed as added
    // non-trivial lines — combined with little fresh behaviour. A pure
    // rename removes old signatures and adds the renamed ones; the
    // counts stay close.
    let added: usize = files
        .iter()
        .map(|f| f.added.iter().filter(|l| !l.trim().is_empty()).count())
        .sum();
    let removed: usize = files
        .iter()
        .map(|f| f.removed.iter().filter(|l| !l.trim().is_empty()).count())
        .sum();
    if added == 0 || removed == 0 {
        return false;
    }
    let ratio = added.min(removed) as f32 / added.max(removed) as f32;
    // >=70% balanced and behaviour-change didn't trip — good
    // indicator of a rename/reorg rather than a feature add.
    ratio >= 0.7 && !behavior_change
}

// ============================================================
// Signal 7: intent hint
// ============================================================

fn extract_intent_hint(files: &[FileChange]) -> Option<String> {
    // Look for the top-of-file `//!` block on a newly added file —
    // that's where a well-documented module states its purpose in a
    // sentence. Fall back to the first `///` block otherwise.
    for file in files {
        if !file.is_new {
            continue;
        }
        if file.is_doc_path || file.is_test_path {
            continue;
        }
        // Gather leading `//!` / `///` lines from the start of the
        // added block (up to first non-comment, non-blank line).
        let mut collected = String::new();
        for line in &file.added {
            let trimmed = line.trim_start();
            if trimmed.is_empty() {
                if collected.is_empty() {
                    continue;
                }
                break;
            }
            if let Some(rest) = trimmed.strip_prefix("//!") {
                collected.push_str(rest.trim());
                collected.push(' ');
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("///") {
                collected.push_str(rest.trim());
                collected.push(' ');
                continue;
            }
            break;
        }
        let collected = collected.trim();
        if !collected.is_empty() {
            return Some(first_sentence(collected, 120));
        }
    }
    None
}

fn first_sentence(text: &str, max_chars: usize) -> String {
    // Cut at the first `. ` boundary or at `max_chars`, whichever
    // comes first. Avoids dumping an entire docblock paragraph into
    // the prompt when all we want is the lead.
    let s: String = text.chars().take(max_chars).collect();
    if let Some(idx) = s.find(". ") {
        return s[..idx + 1].to_string();
    }
    s
}

// ============================================================
// Dominant module
// ============================================================

fn compute_dominant_module(files: &[FileChange]) -> Option<String> {
    // Mode over per-file module tags, but ignore docs / tests so a
    // README edit doesn't dominate a code change. If the result is
    // ambiguous (tie across ≥3 modules) we return None so the model
    // makes its own call instead of committing to a wrong scope.
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for file in files {
        if file.is_doc_path || file.is_test_path {
            continue;
        }
        if let Some(tag) = module_tag(&file.path) {
            *counts.entry(tag).or_insert(0) += 1;
        }
    }
    if counts.is_empty() {
        return None;
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let top = ranked.first()?.clone();
    if ranked.len() >= 2 && ranked[1].1 == top.1 {
        // True tie — bail rather than fabricate a scope.
        return None;
    }
    Some(top.0)
}

fn module_tag(path: &str) -> Option<String> {
    // `src/ui/welcome.rs` → `ui`; `src/clone_auth.rs` → `clone_auth`;
    // `src/ai/tasks/commit_message.rs` → `ai`. We pick the first
    // directory under `src/` so scopes match how the codebase is
    // actually organised (one top-level concept per subdir).
    let path = path.strip_prefix("./").unwrap_or(path);
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() < 2 {
        return None;
    }
    let head = segments[0];
    if head == "src" || head == "lib" || head == "crates" {
        if segments.len() >= 3 {
            return Some(segments[1].to_string());
        }
        // `src/foo.rs` — treat the file stem as its own module tag.
        return Some(short_module(path));
    }
    Some(head.to_string())
}

// ============================================================
// Signal 8: multi-concern segmentation
// ============================================================

/// Partition touched files by "concern" and decide whether the diff
/// really wants to be multiple commits. We intentionally err on the
/// side of NOT fragmenting — a false-positive split recommendation is
/// much more annoying than missing one, because it interrupts the
/// happy path. Triggers:
///
///   * ≥2 code modules each add their own new public API → almost
///     certainly separate features;
///   * one group is docs/tests-only alongside another that isn't →
///     routine split into `feat:` + `docs:`;
///   * otherwise returns `None`.
fn compute_segmentation_advice(
    files: &[FileChange],
    new_public_symbols: &[String],
    docs_only: bool,
    tests_only: bool,
) -> Option<SegmentationAdvice> {
    // If the whole diff is already a single homogeneous concern, no
    // advice needed — the default-path single-message flow handles it.
    if files.len() < 2 || docs_only || tests_only {
        return None;
    }

    let groups = partition_files(files);
    if groups.len() < 2 {
        return None;
    }

    // Trigger 1: multiple code modules each introduce a new public
    // symbol. Check directly by file — cleaner than string-matching
    // the symbol-qualifier prefix, which intentionally differs from
    // the group-partitioning tag (file basename vs. top-level dir).
    let _ = new_public_symbols;
    let mut code_modules_with_new_api = 0usize;
    for g in &groups {
        if g.label == "docs" || g.label == "tests" {
            continue;
        }
        let has = files.iter().any(|f| {
            g.paths.contains(&f.path)
                && !f.is_doc_path
                && !f.is_test_path
                && f.added
                    .iter()
                    .any(|line| extract_pub_symbol(line).is_some())
        });
        if has {
            code_modules_with_new_api += 1;
        }
    }
    if code_modules_with_new_api >= 2 {
        return Some(SegmentationAdvice {
            reason: format!(
                "{code_modules_with_new_api} independent modules each add new public API — \
                 they're almost certainly separate features and should be committed independently."
            ),
            groups,
        });
    }

    // Trigger 2: docs/tests alongside code. Git convention is to keep
    // these separate so `docs:` / `test:` commits aren't buried under
    // an unrelated feature.
    let has_doc_group = groups.iter().any(|g| g.label == "docs");
    let has_test_group = groups.iter().any(|g| g.label == "tests");
    let has_code_group = groups
        .iter()
        .any(|g| g.label != "docs" && g.label != "tests");
    if has_code_group && (has_doc_group || has_test_group) {
        let mut parts = Vec::new();
        if has_doc_group {
            parts.push("documentation");
        }
        if has_test_group {
            parts.push("tests");
        }
        return Some(SegmentationAdvice {
            reason: format!(
                "{} change is mixed with code change — split into separate commits so each \
                 lands under its own Conventional Commits type.",
                parts.join(" and ")
            ),
            groups,
        });
    }

    None
}

fn partition_files(files: &[FileChange]) -> Vec<SegmentationGroup> {
    // Bucket strategy:
    //   * doc files → "docs"
    //   * test paths → "tests"
    //   * everything else → by first-level module tag (`src/foo/…` →
    //     "foo"; `src/bar.rs` → "bar")
    let mut buckets: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for file in files {
        let label = if file.is_doc_path {
            "docs".to_string()
        } else if file.is_test_path {
            "tests".to_string()
        } else {
            module_tag(&file.path).unwrap_or_else(|| "misc".to_string())
        };
        buckets.entry(label).or_default().push(file.path.clone());
    }
    buckets
        .into_iter()
        .map(|(label, paths)| SegmentationGroup { label, paths })
        .collect()
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    const FEAT_DIFF: &str = r#"diff --git a/src/clone_auth.rs b/src/clone_auth.rs
new file mode 100644
--- /dev/null
+++ b/src/clone_auth.rs
@@ -0,0 +1,6 @@
+//! Probe provider accounts to find one that authenticates.
+
+pub fn probe(url: &str) -> Option<String> {
+    None
+}
+
diff --git a/src/clone.rs b/src/clone.rs
--- a/src/clone.rs
+++ b/src/clone.rs
@@ -10,3 +10,6 @@
     let x = 1;
-    let y = 2;
+    if let Some(auth) = probe(url) {
+        return authenticated_clone(url, auth);
+    }
"#;

    const DOCS_DIFF: &str = r#"diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,5 @@
+/// Adds a new doc comment explaining the function's invariants.
+/// Second line of the same comment.
 pub fn foo() {}
"#;

    const TEST_ONLY_DIFF: &str = r#"diff --git a/tests/smoke.rs b/tests/smoke.rs
--- a/tests/smoke.rs
+++ b/tests/smoke.rs
@@ -1,3 +1,4 @@
+#[test]
+fn checks_invariant() { assert_eq!(1, 1); }
"#;

    const FIX_DIFF: &str = r#"diff --git a/src/app.rs b/src/app.rs
--- a/src/app.rs
+++ b/src/app.rs
@@ -10,3 +10,8 @@
-    do_thing(x);
+    // fix: crash when x is empty
+    if x.is_empty() {
+        return Err(anyhow!("empty input"));
+    }
+    do_thing(x);
"#;

    #[test]
    fn feat_diff_flags_new_public_api_and_behavior() {
        let s = analyze(FEAT_DIFF);
        assert!(s.new_public_api, "should detect new pub fn");
        assert!(s.new_public_symbols.iter().any(|s| s.contains("probe")));
        assert!(s.behavior_change);
        assert!(!s.docs_only);
        assert!(!s.tests_only);
        assert_eq!(s.suggested_type(), "feat");
        assert!(s.intent_hint.as_deref().unwrap_or("").contains("Probe"));
    }

    #[test]
    fn docs_only_diff_detects() {
        let s = analyze(DOCS_DIFF);
        assert!(s.docs_only);
        assert!(!s.new_public_api);
        assert_eq!(s.suggested_type(), "docs");
    }

    #[test]
    fn test_only_diff_detects() {
        let s = analyze(TEST_ONLY_DIFF);
        assert!(s.tests_only);
        assert_eq!(s.suggested_type(), "test");
    }

    #[test]
    fn fix_diff_detects_bug_context() {
        let s = analyze(FIX_DIFF);
        assert!(s.bug_error_context);
        // "fix" keyword in comment + new `Err(` construction.
        assert_eq!(s.suggested_type(), "fix");
    }

    #[test]
    fn dominant_module_picks_most_common() {
        let diff = r#"diff --git a/src/ui/a.rs b/src/ui/a.rs
--- a/src/ui/a.rs
+++ b/src/ui/a.rs
@@ -1,1 +1,2 @@
+let x = 1;
 fn a() {}
diff --git a/src/ui/b.rs b/src/ui/b.rs
--- a/src/ui/b.rs
+++ b/src/ui/b.rs
@@ -1,1 +1,2 @@
+let y = 2;
 fn b() {}
diff --git a/src/ai/c.rs b/src/ai/c.rs
--- a/src/ai/c.rs
+++ b/src/ai/c.rs
@@ -1,1 +1,2 @@
+let z = 3;
 fn c() {}
"#;
        let s = analyze(diff);
        assert_eq!(s.dominant_module.as_deref(), Some("ui"));
    }

    #[test]
    fn refactor_like_when_balanced_and_no_new_public() {
        let diff = r#"diff --git a/src/foo.rs b/src/foo.rs
--- a/src/foo.rs
+++ b/src/foo.rs
@@ -1,3 +1,3 @@
-fn old_name() { do_work(); }
-let a = 1;
-let b = 2;
+fn new_name() { do_work(); }
+let a = 1;
+let b = 2;
"#;
        let s = analyze(diff);
        assert!(!s.new_public_api);
        // All adds are either assignment or a non-pub fn — no behaviour
        // change beyond what was already there; refactor-like should
        // kick in.
        assert!(s.refactor_like);
        assert_eq!(s.suggested_type(), "refactor");
    }

    #[test]
    fn prompt_block_contains_headline_fields() {
        let s = analyze(FEAT_DIFF);
        let block = s.render_for_prompt();
        assert!(block.contains("CLASSIFICATION SIGNALS"));
        assert!(block.contains("New public API surface: YES"));
        assert!(block.contains("Suggested type: feat"));
    }

    #[test]
    fn segmentation_flags_multiple_modules_with_new_api() {
        let diff = r#"diff --git a/src/clone_auth.rs b/src/clone_auth.rs
new file mode 100644
--- /dev/null
+++ b/src/clone_auth.rs
@@ -0,0 +1,3 @@
+//! Probe accounts.
+pub fn probe() {}
+
diff --git a/src/ai/change_signals.rs b/src/ai/change_signals.rs
new file mode 100644
--- /dev/null
+++ b/src/ai/change_signals.rs
@@ -0,0 +1,3 @@
+//! Diff shape analysis.
+pub fn analyze() {}
+
"#;
        let s = analyze(diff);
        let advice = s.segmentation_advice.expect("should advise split");
        assert!(advice.reason.contains("independent modules"));
        assert!(advice.groups.len() >= 2);
    }

    #[test]
    fn segmentation_flags_docs_mixed_with_code() {
        let diff = r#"diff --git a/src/foo.rs b/src/foo.rs
--- a/src/foo.rs
+++ b/src/foo.rs
@@ -1,1 +1,2 @@
+pub fn foo() {}
 // ...
diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1,1 +1,2 @@
+New section about foo.
 existing
"#;
        let s = analyze(diff);
        let advice = s.segmentation_advice.expect("should advise split");
        assert!(advice.reason.contains("documentation"));
    }

    #[test]
    fn segmentation_quiet_for_single_concern() {
        // One module, no docs mixed — single commit is fine.
        let diff = r#"diff --git a/src/clone.rs b/src/clone.rs
--- a/src/clone.rs
+++ b/src/clone.rs
@@ -1,1 +1,2 @@
+let x = probe();
 // ...
"#;
        let s = analyze(diff);
        assert!(s.segmentation_advice.is_none());
    }

    #[test]
    fn strip_string_literals_hides_content_from_keyword_match() {
        // The detector's own keyword array contains strings like
        // `"bugfix"` and `"panicked"` — before this fix they tripped
        // the signal on the commit that introduced the detector
        // itself. Stripping quoted content pre-match avoids it.
        let line = r#"    "fixes #", "fix #", "bugfix", "panicked","#;
        let stripped = strip_string_literals(line);
        assert!(!stripped.contains("bugfix"));
        assert!(!stripped.contains("panicked"));
        // Syntax skeleton (the delimiters) survives so code tokens
        // like `const` remain detectable on the same line.
        assert!(stripped.contains('"'));
        assert!(stripped.contains(','));
    }

    #[test]
    fn detect_raw_string_opener_finds_unterminated_opener() {
        assert_eq!(
            detect_raw_string_opener("let s = r#\"hello").as_deref(),
            Some("\"#")
        );
        assert!(detect_raw_string_opener("let s = r#\"hello\"#.into()").is_none());
    }

    #[test]
    fn short_module_uses_parent_dir_for_mod_files() {
        assert_eq!(short_module("src/ai/mod.rs"), "ai");
        assert_eq!(short_module("src/lib.rs"), "src");
        assert_eq!(short_module("src/clone_auth.rs"), "clone_auth");
        assert_eq!(short_module("src/ui/welcome.rs"), "welcome");
    }

    #[test]
    fn pub_symbols_skipped_inside_raw_string_fixture() {
        // Real shape seen in change_signals.rs: a const that embeds a
        // diff fragment as a raw string. Without the skip, the inner
        // `pub fn probe()` leaks as a real public API.
        //
        // Outer delimiter is `##"..."##` so the inner `r#"..."#` that
        // we want to feed to the parser doesn't collide with ours.
        let diff = r##"diff --git a/src/change_signals.rs b/src/change_signals.rs
new file mode 100644
--- /dev/null
+++ b/src/change_signals.rs
@@ -0,0 +1,6 @@
+const FIXTURE: &str = r#"diff --git a/x b/x
+@@ -0,0 +1,1 @@
+pub fn fake_probe() {}
+"#;
+pub fn real_fn() {}
+
"##;
        let s = analyze(diff);
        // `real_fn` is legit; `fake_probe` came from the raw string.
        let symbols_joined = s.new_public_symbols.join(",");
        assert!(symbols_joined.contains("real_fn"));
        assert!(
            !symbols_joined.contains("fake_probe"),
            "fake_probe leaked from raw-string fixture: {symbols_joined}"
        );
    }

    #[test]
    fn pub_use_doesnt_count_as_new_public_api() {
        let diff = r#"diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,1 +1,2 @@
+pub use foo::Bar;
 fn x() {}
"#;
        let s = analyze(diff);
        assert!(!s.new_public_api, "pub use is a re-export, not new surface");
    }
}
