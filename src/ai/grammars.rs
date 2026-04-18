//! GBNF grammars for llama.cpp / Ollama constrained decoding.
//!
//! When the target endpoint supports `grammar`, we pass one of these
//! strings so tiny models can't go off the rails. GBNF is the
//! "extended BNF" dialect that llama.cpp ships — quoted terminals,
//! `|` alternation, `[...]` char classes, `*`/`+`/`?` repetition.
//!
//! Design rules:
//!   * keep rules minimal; each extra rule is another place a 0.5B
//!     model can stall;
//!   * always anchor the root so the model can't prepend prose like
//!     "Sure! Here's your commit message:";
//!   * for JSON-shaped outputs, hand-roll rather than reuse a generic
//!     JSON grammar — field order / required fields catch malformed
//!     completions that a generic grammar would accept.

/// `<type>(<scope>)?: <subject>\n\n<body>?`
///
/// - Type drawn from the Conventional Commits vocabulary.
/// - Scope optional — a SINGLE lowercase token (letters, digits, dash,
///   underscore). No commas, slashes, or spaces: the parser accepts
///   those via tolerant normalisation, but grammar-capable endpoints
///   can just block them outright at decode time.
/// - Subject: up to 72 printable ASCII chars, no newline. We cap at
///   72 via rule repetition rather than a lookahead because GBNF has
///   no lookahead.
/// - Body optional; if present it's separated by a blank line and can
///   contain multiple lines.
pub const GRAMMAR_COMMIT_MSG: &str = r#"
root ::= header ("\n\n" body)?
header ::= commit-type scope? ": " subject
commit-type ::= "feat" | "fix" | "docs" | "style" | "refactor" | "perf" | "test" | "build" | "ci" | "chore" | "revert"
scope ::= "(" [a-z0-9_-]+ ")"
subject ::= subject-char subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char? subject-char?
subject-char ::= [A-Za-z0-9 ._,'\"()\-!?/]
body ::= body-line ("\n" body-line)*
body-line ::= [^\n]{0,120}
"#;

/// One-line stash label, <=60 chars, no newlines.
pub const GRAMMAR_STASH_MSG: &str = r#"
root ::= label
label ::= label-char label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char? label-char?
label-char ::= [A-Za-z0-9 ._,'\"()\-!?/]
"#;

/// Bulleted markdown list, max 8 bullets. Each bullet is a single line.
pub const GRAMMAR_EXPLAIN: &str = r#"
root ::= bullet (bullet){0,7}
bullet ::= "- " line "\n"
line ::= [^\n]+
"#;

/// Strict JSON for commit-composer plans.
///
/// Tight schema on purpose: a 0.5B model given a loose JSON grammar
/// will happily invent extra top-level fields. This forces exactly
/// `{"commits": [...]}` with `{title, body, files}` objects inside.
pub const GRAMMAR_COMPOSER: &str = r#"
root ::= "{" ws "\"commits\"" ws ":" ws "[" ws commit (ws "," ws commit)* ws "]" ws "}"
commit ::= "{" ws "\"title\"" ws ":" ws string ws "," ws "\"body\"" ws ":" ws string ws "," ws "\"files\"" ws ":" ws "[" ws string (ws "," ws string)* ws "]" ws "}"
string ::= "\"" schar* "\""
schar ::= [^"\\] | "\\" ["\\/bfnrt]
ws ::= [ \t\n]*
"#;

/// Strict JSON for conflict-resolution suggestions.
pub const GRAMMAR_CONFLICT: &str = r#"
root ::= "{" ws "\"resolution\"" ws ":" ws resolution-kind ws "," ws "\"merged_text\"" ws ":" ws string ws "," ws "\"rationale\"" ws ":" ws string ws "}"
resolution-kind ::= "\"ours\"" | "\"theirs\"" | "\"merged\""
string ::= "\"" schar* "\""
schar ::= [^"\\] | "\\" ["\\/bfnrt]
ws ::= [ \t\n]*
"#;
