//! Modal file picker for the basket "Focus file" flow.
//!
//! Once the combined diff of a commit-basket is ready, the user can
//! drill into a single file across the selection — "what did these
//! four commits do to `foo.tsx`?". This modal lists every path
//! touched by the combined diff and fuzzy-filters on typed input,
//! with ↑/↓/Enter keyboard navigation mirroring the command palette.
//!
//! WHY a dedicated modal instead of reusing `ui::palette`:
//!   * The palette surfaces *actions*, not per-diff data. Jamming
//!     focus-file rows into it would bloat the action list for every
//!     user, including ones who never touch the basket.
//!   * The candidate list here is bounded by the combined diff's file
//!     count (typically < 200) — we want the modal to appear *anchored
//!     to the combined-diff context* so the user understands the
//!     picker is scoped to the basket selection, not the whole repo.
//!
//! The picker is stateless between openings: we always fuzzy-score
//! the current diff's files with the current query each frame. That
//! keeps the filter result consistent if the underlying diff changes
//! (e.g. the user clears and re-picks commits while the modal is up).

use egui::{Align, Align2, Key, RichText};

/// Per-row height — constant so keyboard nav can keep the selection
/// in view with a single `scroll_to_me` call.
const ROW_HEIGHT: f32 = 24.0;
/// Cap the visible list so a monster combined diff (thousands of
/// changed files) doesn't stretch the modal off-screen. The user
/// narrows with the fuzzy filter.
const MAX_VISIBLE_RESULTS: usize = 40;

/// Modal state for the file picker. `None` on the app means closed.
#[derive(Debug, Default)]
pub struct BasketFocusModalState {
    /// Current fuzzy query. Retained across frames so the input box
    /// keeps its contents while the user types.
    pub query: String,
    /// Index into the *filtered* result list. Clamped each frame in
    /// case the filter shrank the list since last frame.
    pub selected: usize,
}

/// What the modal produced this frame.
#[derive(Debug, Clone)]
pub enum FocusPickerOutcome {
    /// User hit Enter / clicked a row — apply this path as the focus
    /// filter on the workspace's current combined diff.
    Picked(String),
    /// User dismissed the modal (Esc / close). Caller clears the
    /// modal state; any existing focus filter is left alone so the
    /// user can cancel mid-pick without losing their view.
    Cancelled,
}

/// Render the picker. `candidate_paths` is the list of file display
/// paths (e.g. from `RepoDiff::files`), supplied by the caller so the
/// modal has no direct dependency on the diff types.
pub fn show(
    ctx: &egui::Context,
    state: &mut BasketFocusModalState,
    candidate_paths: &[String],
) -> Option<FocusPickerOutcome> {
    let matches = filter(candidate_paths, &state.query);
    if state.selected >= matches.len() {
        state.selected = matches.len().saturating_sub(1);
    }

    let mut outcome: Option<FocusPickerOutcome> = None;

    egui::Window::new("Focus on file")
        .title_bar(true)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, [0.0, 100.0])
        .fixed_size([520.0, 0.0])
        .show(ctx, |ui| {
            ui.weak(
                "Pick a file to filter the combined diff down to just \
                 that path. Type to fuzzy-search.",
            );
            ui.add_space(4.0);

            let prev_query = state.query.clone();
            let input = egui::TextEdit::singleline(&mut state.query)
                .hint_text("Search path…  (↑↓ navigate · Enter · Esc)")
                .desired_width(f32::INFINITY);
            let resp = ui.add(input);

            // Force focus so typing works the moment the modal opens.
            if !resp.has_focus() {
                resp.request_focus();
            }

            // Reset selection to top on any query change — stale
            // highlights across filter mutations are confusing.
            if state.query != prev_query {
                state.selected = 0;
            }

            ui.separator();

            if matches.is_empty() {
                ui.add_space(6.0);
                ui.weak(if state.query.trim().is_empty() {
                    "No files in the combined diff."
                } else {
                    "No matches."
                });
                ui.add_space(6.0);
                return;
            }

            let visible = matches.len().min(MAX_VISIBLE_RESULTS);
            egui::ScrollArea::vertical()
                .max_height(ROW_HEIGHT * visible as f32 + 8.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (idx, path) in matches.iter().take(MAX_VISIBLE_RESULTS).enumerate() {
                        let selected = idx == state.selected;
                        let row = ui.allocate_response(
                            egui::vec2(ui.available_width(), ROW_HEIGHT),
                            egui::Sense::click(),
                        );
                        let painter = ui.painter();
                        if selected {
                            painter.rect_filled(row.rect, 4.0, ui.visuals().selection.bg_fill);
                        } else if row.hovered() {
                            painter.rect_filled(
                                row.rect,
                                4.0,
                                ui.visuals().widgets.hovered.weak_bg_fill,
                            );
                        }
                        let text_color = if selected {
                            ui.visuals().selection.stroke.color
                        } else {
                            ui.visuals().text_color()
                        };
                        painter.text(
                            row.rect.left_center() + egui::vec2(10.0, 0.0),
                            Align2::LEFT_CENTER,
                            path.as_str(),
                            egui::FontId::monospace(13.0),
                            text_color,
                        );
                        if selected {
                            row.scroll_to_me(Some(Align::Center));
                        }
                        if row.clicked() {
                            outcome = Some(FocusPickerOutcome::Picked((*path).clone()));
                        }
                    }
                });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.weak(format!("{} file(s) match", matches.len()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Cancel").clicked() {
                        outcome = Some(FocusPickerOutcome::Cancelled);
                    }
                    if ui
                        .add_enabled(
                            !matches.is_empty(),
                            egui::Button::new(RichText::new("Focus").strong()),
                        )
                        .clicked()
                    {
                        if let Some(p) = matches.get(state.selected) {
                            outcome = Some(FocusPickerOutcome::Picked((*p).clone()));
                        }
                    }
                });
            });
        });

    // Handle keyboard nav outside the window so arrow keys work even
    // when focus wobbles between the text-edit and the window frame.
    let (up, down, enter, esc) = ctx.input(|i| {
        (
            i.key_pressed(Key::ArrowUp),
            i.key_pressed(Key::ArrowDown),
            i.key_pressed(Key::Enter),
            i.key_pressed(Key::Escape),
        )
    });
    if up && state.selected > 0 {
        state.selected -= 1;
    }
    if down && state.selected + 1 < matches.len() {
        state.selected += 1;
    }
    if enter && !matches.is_empty() {
        if let Some(p) = matches.get(state.selected) {
            outcome = Some(FocusPickerOutcome::Picked((*p).clone()));
        }
    }
    if esc {
        outcome = Some(FocusPickerOutcome::Cancelled);
    }

    outcome
}

/// Fuzzy-subsequence filter. Each query character must appear in order
/// in the path (case-insensitive); higher score is better.
///
/// WHY this lives here instead of in `ui::palette`: the palette's
/// scoring is tuned for short action labels ("Checkout main"). File
/// paths are longer and the "start of string" bonus matters less than
/// the "start of path segment" bonus — we want `btn` to score higher
/// on `src/ui/button.rs` (where `btn` is near a segment boundary) than
/// on `src/abort/noise_btn.rs`. Kept intentionally small; if we grow
/// multiple fuzzy scorers across the codebase we should unify them.
pub(crate) fn fuzzy_score(path: &str, query: &str) -> Option<i32> {
    if query.trim().is_empty() {
        return Some(0);
    }
    let path_lc = path.to_lowercase();
    let bytes = path_lc.as_bytes();
    let mut cursor: usize = 0;
    let mut score: i32 = 0;
    let mut last_match: Option<usize> = None;
    for qc in query.to_lowercase().chars() {
        if qc == ' ' {
            continue;
        }
        let mut matched_at: Option<usize> = None;
        while cursor < bytes.len() {
            let c = bytes[cursor] as char;
            cursor += 1;
            if c == qc {
                matched_at = Some(cursor - 1);
                break;
            }
        }
        let idx = matched_at?;
        if let Some(prev) = last_match {
            if idx == prev + 1 {
                score += 8;
            }
        }
        // Boundary bonus: matches right after `/` or at position 0
        // score extra — mirrors how a user usually thinks of paths
        // (segment-first, not whole-string-first).
        let at_boundary = idx == 0 || bytes.get(idx - 1) == Some(&b'/');
        if at_boundary {
            score += 6;
        }
        // Mild distance penalty so earlier matches win ties.
        score -= idx as i32 / 8;
        last_match = Some(idx);
    }
    Some(score)
}

fn filter<'a>(paths: &'a [String], query: &str) -> Vec<&'a String> {
    let q = query.trim();
    let mut scored: Vec<(&String, i32)> = paths
        .iter()
        .filter_map(|p| fuzzy_score(p, q).map(|s| (p, s)))
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored.into_iter().map(|(p, _)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matches_subsequence() {
        assert!(fuzzy_score("src/ui/button.rs", "btn").is_some());
        assert!(fuzzy_score("src/git/basket_ops.rs", "bo").is_some());
    }

    #[test]
    fn fuzzy_rejects_missing_chars() {
        assert!(fuzzy_score("README.md", "xyz").is_none());
    }

    #[test]
    fn segment_boundary_beats_middle() {
        // `button` scores higher on `src/ui/button.rs` (segment start)
        // than on `src/abut/ontology.rs` (mid-word match).
        let boundary = fuzzy_score("src/ui/button.rs", "button").unwrap();
        let middle = fuzzy_score("src/abut/ontology.rs", "button").unwrap_or(i32::MIN);
        assert!(
            boundary > middle,
            "boundary={boundary} middle={middle}"
        );
    }

    #[test]
    fn empty_query_matches_everything() {
        let paths = vec!["a.rs".to_string(), "b.rs".to_string()];
        let result = filter(&paths, "");
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn filter_excludes_non_matches() {
        let paths = vec![
            "src/foo.rs".to_string(),
            "src/bar.rs".to_string(),
            "README.md".to_string(),
        ];
        let result = filter(&paths, "foo");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "src/foo.rs");
    }
}
