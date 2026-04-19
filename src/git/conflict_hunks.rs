//! Conflict-marker parser and composer.
//!
//! The merge-conflict UI used to be a raw text editor over the working-tree
//! copy of a conflicted file: users were expected to hand-delete
//! `<<<<<<< ours`, `=======`, and `>>>>>>> theirs` markers and pick the
//! content they wanted between them. That works, but it's *coarse* — every
//! hunk in the file is edited in the same textarea, there is no "accept this
//! one, keep working on the others" workflow, and a single misplaced
//! keystroke can silently corrupt a marker.
//!
//! This module is the structural half of the per-hunk editor. It turns the
//! merged-text-with-markers into a sequence of typed chunks:
//!
//!   * `ConflictHunkKind::Context(text)` — a run of lines that are the same
//!     on both sides. We keep the text as a single `String` rather than
//!     line-splitting it because the downstream composer only needs to
//!     concatenate chunks back together; keeping trailing newlines verbatim
//!     means round-tripping "no markers anywhere" yields byte-identical
//!     output.
//!   * `ConflictHunkKind::Conflict { ours, theirs, ancestor }` — a block
//!     bounded by `<<<<<<<` / `=======` / `>>>>>>>`. `ancestor` is populated
//!     only when the file was written with diff3-style markers (i.e. the
//!     user has `merge.conflictStyle = diff3` or `zdiff3`), which insert an
//!     `|||||||` line between ours and the classic `=======`.
//!
//! Design notes carried forward from the previous line-scanning parser in
//! `src/ui/conflicts.rs`:
//!
//!   * **Marker lines never appear in the hunk content.** We never emit the
//!     `<<<<<<<` / `|||||||` / `=======` / `>>>>>>>` lines themselves. The
//!     UI gets to decide whether to show them as dividers.
//!   * **Line-based matching, not nested-aware.** If a source file contains
//!     a string literal with the text `"<<<<<<<"` at the start of a line —
//!     for example, a test fixture describing a conflict — the parser will
//!     treat that as a real marker. This is a known, accepted edge case:
//!     real git also gets confused by it, and detecting "this looks like a
//!     marker but is actually code" would require full-language parsing.
//!   * **Malformed input degrades to Context.** If the parser encounters an
//!     unclosed marker (a `<<<<<<<` with no matching `>>>>>>>`) or markers
//!     in the wrong order (e.g. `=======` before any `<<<<<<<`), it returns
//!     a single `Context` chunk wrapping the entire input. The UI detects
//!     this by observing `Conflict` count == 0 even though git reported the
//!     file as conflicted and falls back to the raw textarea editor with an
//!     inline warning.

use std::fmt;

/// One structural unit inside a parsed conflict file.
///
/// Context vs. Conflict is the only axis we care about; we deliberately do
/// not try to expose sub-kinds like "conflict where ours is empty" or
/// "conflict where ours == theirs". The UI renders those the same way — a
/// card with Use-ours / Use-theirs / Use-both / custom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictHunkKind {
    /// A run of lines that are the same on both sides — just context.
    Context(String),
    /// A `<<<<<<<` / `=======` / `>>>>>>>` block. `ancestor` is populated
    /// only for diff3-style markers (`<<<<<<< ours`, `||||||| ancestor`,
    /// `======= ...`, `>>>>>>> theirs`).
    Conflict {
        ours: String,
        theirs: String,
        ancestor: Option<String>,
    },
}

/// Full parse result — the ordered sequence of chunks making up the file.
///
/// `Default::default()` gives you an empty `ParsedConflict` (zero chunks).
/// This is the sensible "no input" value; `parse_conflict_markers("")` also
/// returns an empty result because there is nothing to emit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedConflict {
    pub chunks: Vec<ConflictHunkKind>,
}

impl ParsedConflict {
    /// How many `Conflict` chunks the file contains.
    ///
    /// Callers use this to allocate the per-hunk resolution vector and to
    /// decide whether to fall back to the raw-textarea editor (count == 0
    /// despite git reporting the file as conflicted → parse failure).
    pub fn conflict_count(&self) -> usize {
        self.chunks
            .iter()
            .filter(|c| matches!(c, ConflictHunkKind::Conflict { .. }))
            .count()
    }
}

/// Parse `text` into a `ParsedConflict`.
///
/// The parser is a single pass over `text.split_inclusive('\n')`. We use
/// `split_inclusive` rather than `lines()` because we want to preserve each
/// line's trailing newline verbatim so the composed output round-trips
/// byte-for-byte on context-only inputs.
///
/// A line "starts a marker" if its leading non-CR text begins with exactly
/// 7 of `<`, `|`, `=`, or `>`. Git uses exactly 7 of each marker character;
/// we match the same width to avoid mistakenly catching diff-like output in
/// a code comment (e.g. a 3-`>` quote prefix).
pub fn parse_conflict_markers(text: &str) -> ParsedConflict {
    // Empty input → no chunks. Matches `ParsedConflict::default()`.
    if text.is_empty() {
        return ParsedConflict::default();
    }

    // Intermediate buffers. `context` accumulates lines that belong to the
    // current `Context` chunk; `ours` / `theirs` / `ancestor` buffer inside
    // a conflict block. `state` is the tiny state machine: are we reading
    // context, the ours side, the ancestor side (diff3 only), or the
    // theirs side?
    let mut chunks: Vec<ConflictHunkKind> = Vec::new();
    let mut context = String::new();
    let mut ours = String::new();
    let mut theirs = String::new();
    let mut ancestor = String::new();
    let mut has_ancestor = false;
    let mut state = State::Context;

    for line in text.split_inclusive('\n') {
        // `trimmed` is what we classify against; marker lines have no
        // meaningful trailing whitespace. Keep `line` intact for the
        // *content* so we preserve CR/LF style.
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let marker = classify_marker(trimmed);
        match (state, marker) {
            // ----- Context → open a conflict block -----
            (State::Context, Some(Marker::Open)) => {
                // Flush any accumulated context as a `Context` chunk.
                if !context.is_empty() {
                    chunks.push(ConflictHunkKind::Context(std::mem::take(&mut context)));
                }
                state = State::Ours;
            }
            // Out-of-order markers in Context → malformed input.
            (State::Context, Some(_)) => return fallback(text),
            // Normal context line.
            (State::Context, None) => context.push_str(line),

            // ----- Ours side -----
            (State::Ours, Some(Marker::Ancestor)) => {
                has_ancestor = true;
                state = State::Ancestor;
            }
            (State::Ours, Some(Marker::Split)) => state = State::Theirs,
            (State::Ours, Some(Marker::Close)) | (State::Ours, Some(Marker::Open)) => {
                return fallback(text)
            }
            (State::Ours, None) => ours.push_str(line),

            // ----- Ancestor side (diff3 only) -----
            (State::Ancestor, Some(Marker::Split)) => state = State::Theirs,
            (State::Ancestor, Some(_)) => return fallback(text),
            (State::Ancestor, None) => ancestor.push_str(line),

            // ----- Theirs side -----
            (State::Theirs, Some(Marker::Close)) => {
                chunks.push(ConflictHunkKind::Conflict {
                    ours: std::mem::take(&mut ours),
                    theirs: std::mem::take(&mut theirs),
                    ancestor: if has_ancestor {
                        Some(std::mem::take(&mut ancestor))
                    } else {
                        None
                    },
                });
                has_ancestor = false;
                state = State::Context;
            }
            (State::Theirs, Some(_)) => return fallback(text),
            (State::Theirs, None) => theirs.push_str(line),
        }
    }

    // End of input — if we were still inside a conflict block, that's an
    // unclosed marker → malformed.
    if !matches!(state, State::Context) {
        return fallback(text);
    }

    if !context.is_empty() {
        chunks.push(ConflictHunkKind::Context(context));
    }

    ParsedConflict { chunks }
}

/// Wrap the whole input in a single `Context` chunk. Used whenever we spot
/// a malformed marker sequence so the caller can fall back to the raw
/// editor without the parser pretending it understood the file.
fn fallback(text: &str) -> ParsedConflict {
    ParsedConflict {
        chunks: vec![ConflictHunkKind::Context(text.to_string())],
    }
}

#[derive(Clone, Copy)]
enum State {
    Context,
    Ours,
    Ancestor,
    Theirs,
}

#[derive(Clone, Copy)]
enum Marker {
    /// `<<<<<<<` + optional label.
    Open,
    /// `|||||||` — diff3-style ancestor divider.
    Ancestor,
    /// `=======` — mid-conflict divider between ours/ancestor and theirs.
    Split,
    /// `>>>>>>>` + optional label.
    Close,
}

/// Classify a line — is it a conflict-marker line, and if so which one?
///
/// We require *exactly* 7 of the marker character so three-angle-bracket
/// quoted-text like `>>>` in a docstring doesn't get flagged. The character
/// after the run must be either end-of-line or a space (git uses
/// `<<<<<<< <label>` with a space separator).
fn classify_marker(trimmed: &str) -> Option<Marker> {
    fn matches(line: &str, ch: char) -> bool {
        let mut it = line.chars();
        for _ in 0..7 {
            if it.next() != Some(ch) {
                return false;
            }
        }
        match it.next() {
            None => true,
            Some(' ') => true,
            Some(_) => false,
        }
    }
    if matches(trimmed, '<') {
        Some(Marker::Open)
    } else if matches(trimmed, '|') {
        Some(Marker::Ancestor)
    } else if matches(trimmed, '=') {
        Some(Marker::Split)
    } else if matches(trimmed, '>') {
        Some(Marker::Close)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Per-hunk resolution state + composer
// ---------------------------------------------------------------------------

/// What the user wants to do with a single `Conflict` chunk.
///
/// `Pending` is the initial state — the UI paints it amber and disables the
/// "mark resolved" button until every hunk is non-`Pending`. `Custom` means
/// the user opened the inline editor and typed something; the typed text
/// lives on `HunkResolutionState::custom_text`, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkResolution {
    Pending,
    Ours,
    Theirs,
    Both,
    Custom,
}

impl HunkResolution {
    /// Human-readable tag for the status pill in the UI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Ours => "Ours",
            Self::Theirs => "Theirs",
            Self::Both => "Both",
            Self::Custom => "Custom",
        }
    }
}

impl fmt::Display for HunkResolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Per-hunk UI state — indexed 1:1 with the `Conflict` chunks in a
/// `ParsedConflict`. `custom_text` is always populated; it is ignored
/// unless `resolution == Custom`. We keep it around even when the user
/// toggles back to Ours / Theirs so switching to `Custom` again restores
/// the in-progress edit instead of snapping back to a fresh seed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkResolutionState {
    pub resolution: HunkResolution,
    pub custom_text: String,
}

impl Default for HunkResolutionState {
    fn default() -> Self {
        Self {
            resolution: HunkResolution::Pending,
            custom_text: String::new(),
        }
    }
}

impl Default for HunkResolution {
    fn default() -> Self {
        Self::Pending
    }
}

/// Compose the final file text from a `ParsedConflict` + the user's
/// per-hunk resolutions.
///
/// Returns `Some(text)` only if every `Conflict` chunk has a non-`Pending`
/// resolution. If any hunk is still `Pending`, returns `None` — the UI
/// uses this as the "disable mark-resolved" signal so there's one place
/// the rule lives.
///
/// If `resolutions.len()` is shorter than the number of `Conflict` chunks,
/// the missing trailing entries are treated as `Pending` and the function
/// returns `None`. Longer slices are fine — extra entries are ignored.
pub fn compose_resolved_text(
    parsed: &ParsedConflict,
    resolutions: &[HunkResolutionState],
) -> Option<String> {
    let mut out = String::new();
    let mut conflict_idx = 0usize;
    for chunk in &parsed.chunks {
        match chunk {
            ConflictHunkKind::Context(t) => out.push_str(t),
            ConflictHunkKind::Conflict {
                ours,
                theirs,
                ancestor: _,
            } => {
                let state = resolutions.get(conflict_idx)?;
                match state.resolution {
                    HunkResolution::Pending => return None,
                    HunkResolution::Ours => out.push_str(ours),
                    HunkResolution::Theirs => out.push_str(theirs),
                    HunkResolution::Both => {
                        out.push_str(ours);
                        // If ours didn't end with a newline, insert one so
                        // theirs starts on its own line. This matches the
                        // "Both" behaviour in the existing region card and
                        // avoids silently gluing two lines together.
                        if !ours.ends_with('\n') && !ours.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(theirs);
                    }
                    HunkResolution::Custom => out.push_str(&state.custom_text),
                }
                conflict_idx += 1;
            }
        }
    }
    Some(out)
}

/// Return the text the UI should seed into the inline custom-editor when
/// the user first opens it on a given hunk. The seed is the currently
/// chosen side (`Ours` / `Theirs`) if any, falling back to the ancestor
/// when both sides agree on "start from the common parent", and finally to
/// an empty string. The UI is free to ignore this and just let the user
/// type from scratch; this is only the default seed.
pub fn default_custom_seed(chunk: &ConflictHunkKind, current: HunkResolution) -> String {
    let ConflictHunkKind::Conflict {
        ours,
        theirs,
        ancestor,
    } = chunk
    else {
        return String::new();
    };
    match current {
        HunkResolution::Ours => ours.clone(),
        HunkResolution::Theirs => theirs.clone(),
        HunkResolution::Both => {
            let mut buf = ours.clone();
            if !ours.ends_with('\n') && !ours.is_empty() {
                buf.push('\n');
            }
            buf.push_str(theirs);
            buf
        }
        HunkResolution::Custom | HunkResolution::Pending => {
            // Prefer ancestor when available: it's the common history
            // point, typically a small edit away from whatever the user
            // actually wants. Otherwise fall back to ours as a sensible
            // default starting surface.
            ancestor.clone().unwrap_or_else(|| ours.clone())
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn conflict(ours: &str, theirs: &str) -> ConflictHunkKind {
        ConflictHunkKind::Conflict {
            ours: ours.to_string(),
            theirs: theirs.to_string(),
            ancestor: None,
        }
    }

    #[test]
    fn parses_plain_two_way_marker() {
        let text = "alpha\n<<<<<<< HEAD\nours line\n=======\ntheirs line\n>>>>>>> branch\nomega\n";
        let parsed = parse_conflict_markers(text);
        assert_eq!(parsed.chunks.len(), 3);
        assert_eq!(
            parsed.chunks[0],
            ConflictHunkKind::Context("alpha\n".to_string())
        );
        assert_eq!(parsed.chunks[1], conflict("ours line\n", "theirs line\n"));
        assert_eq!(
            parsed.chunks[2],
            ConflictHunkKind::Context("omega\n".to_string())
        );
        assert_eq!(parsed.conflict_count(), 1);
    }

    #[test]
    fn parses_diff3_style_marker_with_ancestor() {
        // diff3 format: <<<<<<< ours / ||||||| ancestor / ======= / >>>>>>>
        let text = "pre\n\
<<<<<<< HEAD\n\
ours\n\
||||||| merged common ancestors\n\
common\n\
=======\n\
theirs\n\
>>>>>>> feature\n\
post\n";
        let parsed = parse_conflict_markers(text);
        assert_eq!(parsed.chunks.len(), 3);
        match &parsed.chunks[1] {
            ConflictHunkKind::Conflict {
                ours,
                theirs,
                ancestor,
            } => {
                assert_eq!(ours, "ours\n");
                assert_eq!(theirs, "theirs\n");
                assert_eq!(ancestor.as_deref(), Some("common\n"));
            }
            _ => panic!("expected Conflict"),
        }
    }

    #[test]
    fn parses_multiple_consecutive_conflicts_with_context() {
        let text = "A\n<<<<<<<\nO1\n=======\nT1\n>>>>>>>\nB\n<<<<<<<\nO2\n=======\nT2\n>>>>>>>\nC\n";
        let parsed = parse_conflict_markers(text);
        assert_eq!(parsed.chunks.len(), 5);
        assert_eq!(parsed.conflict_count(), 2);
        assert_eq!(
            parsed.chunks[0],
            ConflictHunkKind::Context("A\n".to_string())
        );
        assert_eq!(parsed.chunks[1], conflict("O1\n", "T1\n"));
        assert_eq!(
            parsed.chunks[2],
            ConflictHunkKind::Context("B\n".to_string())
        );
        assert_eq!(parsed.chunks[3], conflict("O2\n", "T2\n"));
        assert_eq!(
            parsed.chunks[4],
            ConflictHunkKind::Context("C\n".to_string())
        );
    }

    #[test]
    fn no_markers_yields_single_context() {
        let text = "one\ntwo\nthree\n";
        let parsed = parse_conflict_markers(text);
        assert_eq!(parsed.chunks.len(), 1);
        assert_eq!(
            parsed.chunks[0],
            ConflictHunkKind::Context(text.to_string())
        );
        assert_eq!(parsed.conflict_count(), 0);
    }

    #[test]
    fn malformed_missing_split_falls_back_to_context() {
        // No `=======` at all — parser should hand back the raw text.
        let text = "<<<<<<<\nonly-ours\n>>>>>>>\n";
        let parsed = parse_conflict_markers(text);
        assert_eq!(parsed.conflict_count(), 0);
        assert_eq!(parsed.chunks.len(), 1);
        assert_eq!(
            parsed.chunks[0],
            ConflictHunkKind::Context(text.to_string())
        );
    }

    #[test]
    fn malformed_unclosed_marker_falls_back() {
        // Open but never closed.
        let text = "<<<<<<<\nours\n=======\ntheirs\n";
        let parsed = parse_conflict_markers(text);
        assert_eq!(parsed.conflict_count(), 0);
        assert_eq!(parsed.chunks.len(), 1);
    }

    #[test]
    fn malformed_orphan_close_falls_back() {
        // `>>>>>>>` with no preceding open — out of order.
        let text = "context\n>>>>>>>\nmore\n";
        let parsed = parse_conflict_markers(text);
        assert_eq!(parsed.conflict_count(), 0);
    }

    #[test]
    fn nested_looking_markers_inside_literals_are_treated_as_markers() {
        // Documented edge case: we do line-based matching, so a string
        // literal whose content begins with 7 `<` at column 0 looks
        // exactly like a real conflict-open marker. git itself doesn't
        // distinguish these either. This test pins the behaviour so a
        // future refactor can't accidentally "fix" it without a spec
        // change.
        let text = "code_before\n<<<<<<<\nA\n=======\nB\n>>>>>>>\ncode_after\n";
        let parsed = parse_conflict_markers(text);
        assert_eq!(parsed.conflict_count(), 1);
    }

    #[test]
    fn marker_char_must_be_exactly_seven() {
        // `>>>` at start of a line (e.g. a quoted reply or blockquote) is
        // NOT a marker — only 3 characters.
        let text = ">>> not a marker\n";
        let parsed = parse_conflict_markers(text);
        assert_eq!(parsed.chunks.len(), 1);
        assert_eq!(parsed.conflict_count(), 0);
    }

    #[test]
    fn compose_returns_none_when_pending() {
        let parsed = parse_conflict_markers(
            "A\n<<<<<<<\nO\n=======\nT\n>>>>>>>\nB\n",
        );
        let resolutions = vec![HunkResolutionState::default()];
        assert!(compose_resolved_text(&parsed, &resolutions).is_none());
    }

    #[test]
    fn compose_ours_reassembles_file() {
        let parsed = parse_conflict_markers(
            "A\n<<<<<<<\nO\n=======\nT\n>>>>>>>\nB\n",
        );
        let resolutions = vec![HunkResolutionState {
            resolution: HunkResolution::Ours,
            custom_text: String::new(),
        }];
        let out = compose_resolved_text(&parsed, &resolutions).unwrap();
        assert_eq!(out, "A\nO\nB\n");
    }

    #[test]
    fn compose_theirs_reassembles_file() {
        let parsed = parse_conflict_markers(
            "A\n<<<<<<<\nO\n=======\nT\n>>>>>>>\nB\n",
        );
        let resolutions = vec![HunkResolutionState {
            resolution: HunkResolution::Theirs,
            custom_text: String::new(),
        }];
        let out = compose_resolved_text(&parsed, &resolutions).unwrap();
        assert_eq!(out, "A\nT\nB\n");
    }

    #[test]
    fn compose_both_concatenates_with_newline_bridge() {
        // Ours lacks a trailing newline on the *segment* (real files
        // always terminate lines, but the hunk body may or may not).
        // Composer should bridge with a newline so theirs starts cleanly.
        let parsed = ParsedConflict {
            chunks: vec![
                ConflictHunkKind::Context("A\n".into()),
                ConflictHunkKind::Conflict {
                    ours: "O".into(),
                    theirs: "T\n".into(),
                    ancestor: None,
                },
                ConflictHunkKind::Context("B\n".into()),
            ],
        };
        let resolutions = vec![HunkResolutionState {
            resolution: HunkResolution::Both,
            custom_text: String::new(),
        }];
        let out = compose_resolved_text(&parsed, &resolutions).unwrap();
        assert_eq!(out, "A\nO\nT\nB\n");
    }

    #[test]
    fn compose_custom_uses_custom_text() {
        let parsed = parse_conflict_markers(
            "A\n<<<<<<<\nO\n=======\nT\n>>>>>>>\nB\n",
        );
        let resolutions = vec![HunkResolutionState {
            resolution: HunkResolution::Custom,
            custom_text: "CUSTOM\n".to_string(),
        }];
        let out = compose_resolved_text(&parsed, &resolutions).unwrap();
        assert_eq!(out, "A\nCUSTOM\nB\n");
    }

    #[test]
    fn compose_mixed_multiple_hunks() {
        let parsed = parse_conflict_markers(
            "A\n<<<<<<<\nO1\n=======\nT1\n>>>>>>>\nB\n<<<<<<<\nO2\n=======\nT2\n>>>>>>>\nC\n",
        );
        let resolutions = vec![
            HunkResolutionState {
                resolution: HunkResolution::Ours,
                custom_text: String::new(),
            },
            HunkResolutionState {
                resolution: HunkResolution::Theirs,
                custom_text: String::new(),
            },
        ];
        let out = compose_resolved_text(&parsed, &resolutions).unwrap();
        assert_eq!(out, "A\nO1\nB\nT2\nC\n");
    }

    #[test]
    fn compose_no_conflicts_passthrough() {
        let parsed = parse_conflict_markers("just context\nno markers here\n");
        let out = compose_resolved_text(&parsed, &[]).unwrap();
        assert_eq!(out, "just context\nno markers here\n");
    }

    #[test]
    fn default_custom_seed_prefers_current_side() {
        let chunk = conflict("O\n", "T\n");
        assert_eq!(default_custom_seed(&chunk, HunkResolution::Ours), "O\n");
        assert_eq!(default_custom_seed(&chunk, HunkResolution::Theirs), "T\n");
    }

    #[test]
    fn default_custom_seed_pending_prefers_ancestor_when_present() {
        let chunk = ConflictHunkKind::Conflict {
            ours: "O\n".into(),
            theirs: "T\n".into(),
            ancestor: Some("A\n".into()),
        };
        assert_eq!(
            default_custom_seed(&chunk, HunkResolution::Pending),
            "A\n"
        );
    }
}
