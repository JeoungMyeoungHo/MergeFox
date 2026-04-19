//! Thin minimap column for the diff viewer.
//!
//! Why a minimap belongs on the diff panel: once a file's diff is
//! longer than a screenful, the scrollbar becomes a very poor overview.
//! You can see *that* there's more to read, but not *where* the
//! interesting edits live. Rendering a one-pixel-tall bar per diff row,
//! coloured by line kind, turns the right edge of the panel into a
//! density map — huge deletion blocks, tiny inline tweaks, and
//! untouched context all become visually distinguishable at a glance.
//!
//! Scope notes:
//!
//!   * **Row-aligned, not line-aligned.** We mirror the row sequence
//!     the diff panel actually paints (flattened across hunks). This
//!     keeps the cursor-to-diff mapping trivial and matches what the
//!     user perceives: a gap in the minimap is a gap in the diff
//!     panel's scroll contents, not in the underlying file.
//!   * **Clamped row count.** For very large diffs `row_h` naturally
//!     collapses below one pixel; we floor at ~0.5 px and let egui's
//!     subpixel coverage merge adjacent same-kind rows into a solid
//!     band.
//!   * **Click/drag scrolls.** The viewport overlay is the "you are
//!     here" indicator. Clicking anywhere in the strip centres the
//!     viewport on that Y position next frame; dragging does the same
//!     continuously so the user can scrub.
//!   * **No labels, no per-file splits.** The minimap is a low-effort
//!     high-value summary. Anything richer (cross-file heatmap, hunk
//!     boundaries as ticks) is a follow-up — keep the first pass tiny.

use egui::{Color32, Rect, Response, Sense, Stroke, Vec2};

use crate::git::{FileDiff, FileKind, LineKind};

/// One row in the minimap strip. We intentionally don't store line
/// text or numbers — the only thing the painter cares about is kind,
/// and keeping the struct tiny lets us compute it lazily per-file
/// without adding a measurable allocation tax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinimapRow {
    pub kind: LineKind,
}

/// Build the flat row list for a file diff. One entry per rendered
/// line (all hunks concatenated, hunk headers emitted as Meta rows so
/// they appear as faint separators rather than vanishing gaps).
///
/// Non-text files — binary, image, too-large — have no hunks and
/// therefore produce an empty row list. The caller uses that as the
/// signal to hide the minimap column entirely.
pub fn rows_for_file(diff: &FileDiff) -> Vec<MinimapRow> {
    let FileKind::Text { hunks, .. } = &diff.kind else {
        return Vec::new();
    };
    let approx = hunks.iter().map(|h| h.lines.len() + 1).sum::<usize>();
    let mut rows = Vec::with_capacity(approx);
    for hunk in hunks {
        // Hunk header row: a faint separator that mirrors the visible
        // header row in the diff panel so the minimap y-coordinate
        // stays 1:1 with the scroll-area y-coordinate.
        rows.push(MinimapRow {
            kind: LineKind::Meta,
        });
        for line in &hunk.lines {
            rows.push(MinimapRow { kind: line.kind });
        }
    }
    rows
}

/// Render the minimap column.
///
/// * `scroll_y` — the current scroll position of the diff panel, in
///   diff-panel pixels. The overlay rectangle is positioned from this.
/// * `total_y` — the total scrollable height of the diff panel (rows ×
///   row-height). Used to convert minimap-y into diff-y.
/// * `viewport_h` — the visible portion of the diff panel. Feeds the
///   overlay rectangle's height so the user can see what fraction of
///   the file is currently on screen.
/// * `width` — how wide to allocate the strip. The caller typically
///   picks 14–18 px.
///
/// Returns `Some(new_scroll_y)` only on the frame the user interacts
/// (click or drag). The caller applies the returned offset to the
/// diff scroll area via `ScrollArea::scroll_offset` on the next frame.
pub fn show(
    ui: &mut egui::Ui,
    rows: &[MinimapRow],
    scroll_y: f32,
    total_y: f32,
    viewport_h: f32,
    width: f32,
) -> Option<f32> {
    if rows.is_empty() {
        return None;
    }

    let available_h = ui.available_height().max(1.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, available_h), Sense::click_and_drag());
    let painter = ui.painter_at(rect);

    // Background — slightly darker than the panel so the strip reads
    // as a distinct column even when the diff is all context (all
    // rows would otherwise be transparent).
    painter.rect_filled(
        rect,
        0.0,
        ui.visuals().extreme_bg_color.gamma_multiply(0.6),
    );

    // Per-row stripe height. Once this drops below 1 px we paint
    // fractional heights and rely on the renderer's alpha coverage to
    // bundle adjacent rows into a solid band.
    let row_h = (available_h / rows.len() as f32).max(0.5);
    for (i, row) in rows.iter().enumerate() {
        let color = color_for_kind(row.kind);
        if color == Color32::TRANSPARENT {
            continue;
        }
        let y = rect.top() + (i as f32) * (available_h / rows.len() as f32);
        let stripe = Rect::from_min_size(
            egui::pos2(rect.left() + 1.0, y),
            Vec2::new(width - 2.0, row_h),
        );
        painter.rect_filled(stripe, 0.0, color);
    }

    // Viewport overlay — the translucent rectangle showing "this
    // slice of the diff is what's currently on screen". Clamped so
    // it never extends past the strip even if the caller hands us a
    // viewport that's larger than the total height (empty / tiny
    // diffs during the first frame).
    paint_viewport_overlay(&painter, rect, scroll_y, total_y, viewport_h);

    // Scroll-on-interact: click places the viewport centre on the
    // pointer, drag does the same continuously so the user can scrub
    // without letting go.
    scroll_from_interaction(&response, rect, total_y, viewport_h)
}

fn color_for_kind(kind: LineKind) -> Color32 {
    // Opaque colours so the minimap reads clearly even over the
    // faint panel background. The diff panel itself uses translucent
    // backgrounds to keep context legible; here we want the summary
    // to pop.
    match kind {
        LineKind::Add => Color32::from_rgb(90, 180, 120),
        LineKind::Remove => Color32::from_rgb(220, 110, 110),
        LineKind::Context => Color32::from_gray(90),
        LineKind::Meta => Color32::from_rgb(90, 130, 170),
    }
}

fn paint_viewport_overlay(
    painter: &egui::Painter,
    rect: Rect,
    scroll_y: f32,
    total_y: f32,
    viewport_h: f32,
) {
    if total_y <= 0.0 {
        return;
    }
    let visible_frac = (viewport_h / total_y).clamp(0.0, 1.0);
    let top_frac = (scroll_y / total_y).clamp(0.0, 1.0);
    let overlay_h = (rect.height() * visible_frac).max(4.0);
    let overlay_top = rect.top() + rect.height() * top_frac;
    let overlay_top = overlay_top.min(rect.bottom() - overlay_h);
    let overlay = Rect::from_min_size(
        egui::pos2(rect.left(), overlay_top),
        Vec2::new(rect.width(), overlay_h),
    );
    painter.rect_filled(overlay, 0.0, Color32::from_rgba_unmultiplied(255, 255, 255, 28));
    painter.rect_stroke(
        overlay,
        0.0,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 64)),
    );
}

fn scroll_from_interaction(
    response: &Response,
    rect: Rect,
    total_y: f32,
    viewport_h: f32,
) -> Option<f32> {
    // We only emit a new scroll target on frames the user actually
    // touched the strip — no click, no drag, no hover motion alone
    // (hovering shouldn't snap the diff).
    if !(response.clicked() || response.dragged()) {
        return None;
    }
    let pointer_y = response.interact_pointer_pos()?.y;
    let frac = ((pointer_y - rect.top()) / rect.height()).clamp(0.0, 1.0);
    // Centre the viewport on the pointer rather than anchoring its
    // top there — clicking the middle of the minimap should leave
    // the clicked line roughly in the middle of the panel, which
    // matches the "this is where I want to read" reflex.
    let target_center = frac * total_y;
    let new_scroll = (target_center - viewport_h * 0.5).max(0.0);
    let max_scroll = (total_y - viewport_h).max(0.0);
    Some(new_scroll.min(max_scroll))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{DeltaStatus, DiffLine, FileKind, Hunk};

    fn make_file(kinds: &[LineKind]) -> FileDiff {
        let lines = kinds
            .iter()
            .map(|k| DiffLine {
                kind: *k,
                content: String::new(),
                old_lineno: None,
                new_lineno: None,
            })
            .collect();
        let hunk = Hunk {
            header: "@@ -1 +1 @@".into(),
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
            lines,
        };
        FileDiff {
            old_path: None,
            new_path: None,
            status: DeltaStatus::Modified,
            kind: FileKind::Text {
                hunks: vec![hunk],
                lines_added: 0,
                lines_removed: 0,
                truncated: false,
            },
            old_size: 0,
            new_size: 0,
            old_oid: None,
            new_oid: None,
        }
    }

    #[test]
    fn empty_for_non_text() {
        let binary = FileDiff {
            old_path: None,
            new_path: None,
            status: DeltaStatus::Modified,
            kind: FileKind::Binary,
            old_size: 0,
            new_size: 0,
            old_oid: None,
            new_oid: None,
        };
        assert!(rows_for_file(&binary).is_empty());
    }

    #[test]
    fn one_row_per_line_plus_hunk_header() {
        let file = make_file(&[LineKind::Context, LineKind::Add, LineKind::Remove]);
        let rows = rows_for_file(&file);
        // 1 hunk header + 3 lines
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].kind, LineKind::Meta);
        assert_eq!(rows[1].kind, LineKind::Context);
        assert_eq!(rows[2].kind, LineKind::Add);
        assert_eq!(rows[3].kind, LineKind::Remove);
    }

    #[test]
    fn rows_preserve_kind_order_across_hunks() {
        // Two hunks, each contributing a header + a Remove row.
        let base = make_file(&[LineKind::Remove]);
        let mut extended = base.clone();
        if let FileKind::Text { hunks, .. } = &mut extended.kind {
            let extra = hunks[0].clone();
            hunks.push(extra);
        }
        let rows = rows_for_file(&extended);
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows.iter().map(|r| r.kind).collect::<Vec<_>>(),
            vec![
                LineKind::Meta,
                LineKind::Remove,
                LineKind::Meta,
                LineKind::Remove,
            ]
        );
    }
}
