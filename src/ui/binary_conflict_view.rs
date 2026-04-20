//! Binary-conflict resolution card.
//!
//! Scope
//! -----
//! When `git` reports a file as conflicted and the file is binary
//! (images, executables, Photoshop documents, FBX meshes, compiled
//! archives, …), the text-hunk editor can't render anything useful —
//! there's no line structure, UTF-8 decode fails, and trying to overlay
//! `<<<<<<<` markers on an opaque byte blob would corrupt it. The whole-
//! file "Use ours / Use theirs" control that used to live in the main
//! conflict panel is the minimum needed to ship, but for the two most
//! common real-world cases it's not enough:
//!
//!   * **Images** — the user wants to *see* both sides before picking.
//!     A PNG / JPG / EXR preview is usually enough to tell which side
//!     is "the right one".
//!   * **Opaque binary formats** (PSD, FBX, compiled DLLs, archives) —
//!     the user wants to keep *both* versions on disk under different
//!     names (`file.ours.psd`, `file.theirs.psd`) so they can diff or
//!     merge them in an external tool, then decide what the canonical
//!     content should be.
//!
//! This module renders the card itself. It is deliberately **pure UI**:
//! no filesystem writes, no `Repo` calls. The caller translates the
//! returned `BinaryConflictIntent` into the matching backend action
//! (`Repo::resolve_conflict_choice`, `Repo::resolve_conflict_keep_both`,
//! or a "export-and-open" export).
//!
//! Layout
//! ------
//! * **Header** — full path + size summary for ours / theirs.
//! * **Twin preview panes** (`ours_long` left, `theirs_long` right). If
//!   the file is image-like (as determined by `file_preview::FormatKind`),
//!   we request the full-resolution preview and blit the resulting
//!   texture. For non-image binary formats we draw a typed badge + the
//!   short OID + byte size.
//! * **Four inline buttons**:
//!   * `Use ours` / `Use theirs` — the original whole-file pick.
//!   * `Keep both (original = ours)` / `Keep both (original = theirs)`
//!     — dispatch `KeepBoth { keep_as_main }` so the caller writes the
//!     *other* side to disk under a sibling name and stages the
//!     original.
//! * **Binary-only hint** below the buttons when the file is NOT image-
//!   like, telling the user there's no in-process preview but they can
//!   still open each side externally.

use std::path::Path;

use egui::{Color32, RichText, Stroke};

use crate::git::{ConflictBlob, ConflictChoice, ConflictEntry};
use crate::ui::file_preview::{
    FormatKind, PreviewManager, PreviewMode, PreviewState, THUMB_MAX_DIM,
};

/// User-driven action the card can emit this frame. The card is
/// stateless — each call to [`render`] returns at most one intent for
/// the outer controller to route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryConflictIntent {
    /// Resolve by taking the `ours` side as the canonical file and
    /// discarding `theirs`.
    UseOurs,
    /// Resolve by taking the `theirs` side as the canonical file and
    /// discarding `ours`.
    UseTheirs,
    /// Keep BOTH sides on disk: the chosen `keep_as_main` side writes
    /// back to the original path (and gets staged), the other side is
    /// written to a sibling path with a `.ours`/`.theirs` infix so the
    /// user can pick up a manual merge externally afterwards.
    KeepBoth { keep_as_main: ConflictChoice },
    /// Export one side to a temp file and open it with the platform's
    /// default viewer. The UI does *not* write to disk itself — the
    /// caller owns the export step.
    OpenExternal { side: ConflictChoice },
}

/// Short labels for the two sides of the merge. We keep this struct
/// deliberately minimal (two strings) so callers that already have
/// richer [`super::conflicts::SideLabels`] state don't have to thread
/// long-form descriptions through here; the card only needs the short
/// form for the header chips.
pub struct SideLabels {
    /// e.g. `"ours (feat/redesign)"` — shown on the left pane header and
    /// inside the `Use ours` button.
    pub ours_short: String,
    /// e.g. `"theirs (main)"` — symmetric pair.
    pub theirs_short: String,
}

/// Accent palette — intentionally a subset of the main conflict panel's
/// palette so the two cards read as part of the same visual family.
/// Duplicated (instead of re-exporting) so this module doesn't reach
/// into a private `palette` mod inside `conflicts.rs`.
mod palette {
    use egui::Color32;
    pub const OURS: Color32 = Color32::from_rgb(86, 156, 214);
    pub const THEIRS: Color32 = Color32::from_rgb(220, 140, 60);
    pub const MUTED: Color32 = Color32::from_rgb(170, 170, 170);
    pub const CARD_FILL: Color32 = Color32::from_rgb(38, 40, 44);
    pub const HINT_BG: Color32 = Color32::from_rgb(48, 48, 54);
}

/// Render the card. Returns the single most-recent intent emitted this
/// frame, or `None` if the user didn't click anything.
///
/// The card assumes `entry.is_binary` is true; rendering it for a text
/// conflict would still compile but displaces the text-hunk editor and
/// is an API misuse. Callers should branch on `is_binary` before
/// delegating here (the `conflicts.rs` integration does exactly that).
pub fn render(
    ui: &mut egui::Ui,
    entry: &ConflictEntry,
    labels: &SideLabels,
) -> Option<BinaryConflictIntent> {
    let mut intent: Option<BinaryConflictIntent> = None;

    // Header: path + side sizes. The "ours A · theirs B" chip is useful
    // on its own — if the two sides have dramatically different sizes
    // (e.g. a 12 KB placeholder vs. a 4 MB real texture) the user can
    // usually tell at a glance which side is the "real" content without
    // even looking at the preview.
    ui.horizontal(|ui| {
        ui.label(RichText::new(format_path(&entry.path)).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.weak(format_size_summary(
                entry.ours.as_ref(),
                entry.theirs.as_ref(),
            ));
        });
    });

    ui.add_space(4.0);

    // Twin previews — equal-width columns. Each column either renders
    // an image preview (when decodable) or a typed placeholder.
    ui.columns(2, |columns| {
        render_side_pane(
            &mut columns[0],
            &entry.path,
            &labels.ours_short,
            palette::OURS,
            entry.ours.as_ref(),
            ConflictChoice::Ours,
            &mut intent,
        );
        render_side_pane(
            &mut columns[1],
            &entry.path,
            &labels.theirs_short,
            palette::THEIRS,
            entry.theirs.as_ref(),
            ConflictChoice::Theirs,
            &mut intent,
        );
    });

    ui.add_space(6.0);

    // Action row.
    //
    // Keep-both button text includes which side is the "main" (original
    // path) because the intent is otherwise ambiguous — `Keep both` on
    // its own would require the user to guess which file survives under
    // the original name.
    ui.horizontal_wrapped(|ui| {
        let ours_btn = egui::Button::new(
            RichText::new(format!("Use {}", labels.ours_short))
                .color(Color32::WHITE)
                .strong(),
        )
        .fill(palette::OURS);
        ui.add_enabled_ui(entry.ours.is_some(), |ui| {
            if ui
                .add(ours_btn)
                .on_hover_text(format!(
                    "Resolve the whole file to the {} version, discarding {}.",
                    labels.ours_short, labels.theirs_short
                ))
                .clicked()
            {
                intent = Some(BinaryConflictIntent::UseOurs);
            }
        });

        let theirs_btn = egui::Button::new(
            RichText::new(format!("Use {}", labels.theirs_short))
                .color(Color32::WHITE)
                .strong(),
        )
        .fill(palette::THEIRS);
        ui.add_enabled_ui(entry.theirs.is_some(), |ui| {
            if ui
                .add(theirs_btn)
                .on_hover_text(format!(
                    "Resolve the whole file to the {} version, discarding {}.",
                    labels.theirs_short, labels.ours_short
                ))
                .clicked()
            {
                intent = Some(BinaryConflictIntent::UseTheirs);
            }
        });

        ui.separator();

        let keep_both_tip = "Resolves the conflict but leaves both versions on disk \
             with different names so you can diff / merge manually.";

        // Keep both with ours as the canonical file — writes `theirs`
        // out with the infix and stages `ours` at the original path.
        ui.add_enabled_ui(
            entry.ours.is_some() && entry.theirs.is_some(),
            |ui| {
                if ui
                    .button(
                        RichText::new(format!(
                            "Keep both (original = {})",
                            labels.ours_short
                        ))
                        .strong(),
                    )
                    .on_hover_text(keep_both_tip)
                    .clicked()
                {
                    intent = Some(BinaryConflictIntent::KeepBoth {
                        keep_as_main: ConflictChoice::Ours,
                    });
                }
            },
        );

        ui.add_enabled_ui(
            entry.ours.is_some() && entry.theirs.is_some(),
            |ui| {
                if ui
                    .button(
                        RichText::new(format!(
                            "Keep both (original = {})",
                            labels.theirs_short
                        ))
                        .strong(),
                    )
                    .on_hover_text(keep_both_tip)
                    .clicked()
                {
                    intent = Some(BinaryConflictIntent::KeepBoth {
                        keep_as_main: ConflictChoice::Theirs,
                    });
                }
            },
        );
    });

    // Non-image binary hint. Previewable formats (PNG/JPG/PSD/EXR/…)
    // already show a visual preview in the twin panes, so there's no
    // need to tell the user "hey, this is binary". For opaque formats
    // (FBX / compiled binaries / archives) we call that out explicitly
    // because the twin panes fall back to a typed badge and the user
    // might otherwise wonder why there's no preview.
    if !is_image_like(&entry.path) {
        ui.add_space(6.0);
        egui::Frame::none()
            .fill(palette::HINT_BG)
            .rounding(egui::Rounding::same(4.0))
            .inner_margin(egui::Margin::symmetric(8.0, 6.0))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(
                        "This file is binary and can't be previewed. \
                         Use the buttons below or open the sides externally.",
                    )
                    .color(palette::MUTED)
                    .small(),
                );
                // Inline per-side "open externally" buttons. Placed here
                // rather than in the action row above so the keep-both
                // button line stays readable on narrow windows.
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(entry.ours.is_some(), |ui| {
                        if ui
                            .small_button(format!("Open {} externally", labels.ours_short))
                            .on_hover_text(
                                "Export this side to a temp file and open it with the \
                                 system default app.",
                            )
                            .clicked()
                        {
                            intent = Some(BinaryConflictIntent::OpenExternal {
                                side: ConflictChoice::Ours,
                            });
                        }
                    });
                    ui.add_enabled_ui(entry.theirs.is_some(), |ui| {
                        if ui
                            .small_button(format!("Open {} externally", labels.theirs_short))
                            .on_hover_text(
                                "Export this side to a temp file and open it with the \
                                 system default app.",
                            )
                            .clicked()
                        {
                            intent = Some(BinaryConflictIntent::OpenExternal {
                                side: ConflictChoice::Theirs,
                            });
                        }
                    });
                });
            });
    }

    intent
}

/// Draw a single side's preview pane (ours or theirs).
///
/// The pane has a fixed minimum height so the two columns line up
/// regardless of whether one side decoded and the other fell back to
/// a placeholder. Clicking on the pane itself does nothing — the
/// buttons below the card are the primary affordance; the preview is
/// informational.
fn render_side_pane(
    ui: &mut egui::Ui,
    path: &Path,
    title: &str,
    accent: Color32,
    blob: Option<&ConflictBlob>,
    _which: ConflictChoice,
    _intent: &mut Option<BinaryConflictIntent>,
) {
    egui::Frame::group(ui.style())
        .fill(palette::CARD_FILL)
        .stroke(Stroke::new(1.5, accent))
        .rounding(egui::Rounding::same(6.0))
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
        .show(ui, |ui| {
            ui.set_min_width(280.0);
            ui.set_min_height(180.0);

            // Side header: title + side stats.
            ui.horizontal(|ui| {
                ui.colored_label(accent, RichText::new(title).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(b) = blob {
                        ui.weak(format_size(b.size));
                        if let Some(oid) = b.oid {
                            ui.weak(RichText::new(short_oid(&oid)).monospace().small());
                        }
                    } else {
                        ui.weak("absent");
                    }
                });
            });
            ui.separator();

            match blob {
                None => {
                    // File was added on one side only / deleted on this
                    // one. We still render the pane so the layout stays
                    // symmetric, but there's nothing to preview.
                    ui.weak("No content on this side (file added or deleted elsewhere).");
                }
                Some(b) => {
                    // Image-like? Try to drive the preview manager.
                    // Everything else gets a typed placeholder — same
                    // fallback file-list rows use for e.g. FBX models.
                    let ext = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    let kind = FormatKind::from_ext(&ext);
                    match kind {
                        FormatKind::Image | FormatKind::Psd => {
                            render_preview_if_blob_bytes(ui, b, &ext, accent);
                        }
                        FormatKind::OpaqueAsset(label) => {
                            render_placeholder_badge(ui, label, accent);
                        }
                        FormatKind::Unknown => {
                            render_placeholder_badge(ui, "binary file", accent);
                        }
                    }
                }
            }
        });
}

/// Ask the preview manager to decode this blob's bytes. Requires we
/// already have the bytes in memory — which is *not* the case for
/// `ConflictBlob` (we only carry size + oid + optional utf-8 text).
/// Without the raw bytes we can't hand them to [`PreviewManager`]. We
/// gracefully degrade to a sized badge so the pane still feels alive.
///
/// The trade-off is deliberate: duplicating the blob bytes inside
/// `ConflictBlob` would bloat `conflict_entries()` for every file on
/// every refresh, and the UI only actually wants the bytes when a
/// *binary* entry is selected. A richer design would lazily fetch
/// bytes on selection; that's follow-up work. For the first pass the
/// OID + typed label is enough to differentiate ours from theirs.
fn render_preview_if_blob_bytes(
    ui: &mut egui::Ui,
    blob: &ConflictBlob,
    ext: &str,
    accent: Color32,
) {
    // We don't have raw bytes on `ConflictBlob` — see doc comment
    // above. Try the preview manager's cache in case some other path
    // (e.g. a prior diff-pane render for the same OID) already decoded
    // this blob; otherwise fall through to a placeholder that still
    // tells the user "this is a previewable image, we just haven't
    // loaded it yet".
    if let Some(oid) = blob.oid {
        let mgr = PreviewManager::global();
        let key = crate::ui::file_preview::PreviewKey {
            identity: crate::ui::file_preview::PreviewIdentity::Blob(oid),
            mode: PreviewMode::Full,
        };
        if let Some(tex) = mgr.texture_for(ui.ctx(), &key, "binary-conflict") {
            let size = tex.size_vec2();
            let max_w = ui.available_width() - 4.0;
            let max_h = 240.0f32;
            let scale = (max_w / size.x)
                .min(max_h / size.y)
                .min(1.0);
            let draw = egui::vec2(size.x * scale, size.y * scale);
            ui.add(egui::Image::from_texture(&tex).fit_to_exact_size(draw));
            return;
        }
        let _ = ext;
        let _ = THUMB_MAX_DIM;
    }
    // Fall-through: decoded texture is not in cache. Draw a typed
    // placeholder. This is the common case today because we don't
    // plumb blob bytes through `ConflictBlob`.
    render_placeholder_badge(ui, "image preview not loaded", accent);
}

/// Typed badge rendered when the side isn't image-previewable in
/// process. Matches the look the main file list uses for FBX / Blender
/// / archive rows so the visual language stays consistent.
fn render_placeholder_badge(ui: &mut egui::Ui, label: &str, accent: Color32) {
    ui.vertical_centered(|ui| {
        ui.add_space(24.0);
        ui.label(RichText::new("●").color(accent).size(36.0));
        ui.add_space(6.0);
        ui.label(RichText::new(label).color(palette::MUTED).small());
        ui.add_space(24.0);
    });
}

/// Extension-driven "is this an image the user expects to see inline?".
/// Mirrors the `FormatKind::Image` / `Psd` bucket in `file_preview` —
/// every format that path can thumbnail also gets the inline preview
/// in this card.
pub fn is_image_like(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        FormatKind::from_ext(&ext),
        FormatKind::Image | FormatKind::Psd
    )
}

/// Produce the sibling filename for a Keep-both resolution. For a file
/// named `foo.psd` with `side = Theirs` this returns `foo.theirs.psd`.
///
/// Edge cases:
///   * **No extension** (`README`, `Makefile`) — append `.<side>` with
///     no suffix: `README.ours`.
///   * **Multiple dots** (`archive.tar.gz`) — only the final component
///     is treated as the extension, matching `Path::extension()` and
///     Unix convention: `archive.tar.ours.gz`.
///   * **Hidden files** (`.gitattributes`) — `Path::extension` returns
///     `None` for a leading-dot-only filename, so this falls into the
///     "no extension" branch: `.gitattributes.ours`.
pub fn sibling_side_name(path: &Path, side: ConflictChoice) -> std::path::PathBuf {
    let side_tag = match side {
        ConflictChoice::Ours => "ours",
        ConflictChoice::Theirs => "theirs",
    };
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let file_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let ext = path.extension().and_then(|e| e.to_str());

    let file_name = match ext {
        Some(ext) if !ext.is_empty() => {
            // Special case: `Path::extension` on a hidden-only file
            // (`.gitattributes`) returns `Some("gitattributes")` and
            // `file_stem` returns `Some("")` — check for that so we
            // don't emit `".ours.gitattributes"` which would hide the
            // file under a new name. Fall back to the "no extension"
            // branch for dotfiles.
            if file_stem.is_empty() {
                // Dotfile like `.gitattributes` — stem is empty, treat
                // the whole name as an unextended identifier.
                let full = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                format!("{full}.{side_tag}")
            } else {
                format!("{file_stem}.{side_tag}.{ext}")
            }
        }
        _ => {
            // No extension at all.
            let full = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if full.is_empty() {
                // Defensive: should not happen for real paths.
                format!("file.{side_tag}")
            } else {
                format!("{full}.{side_tag}")
            }
        }
    };

    if parent.as_os_str().is_empty() {
        std::path::PathBuf::from(file_name)
    } else {
        parent.join(file_name)
    }
}

fn format_path(path: &Path) -> String {
    path.display().to_string()
}

fn format_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = KB * 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{bytes} B")
    }
}

fn format_size_summary(ours: Option<&ConflictBlob>, theirs: Option<&ConflictBlob>) -> String {
    let o = ours
        .map(|b| format_size(b.size))
        .unwrap_or_else(|| "absent".into());
    let t = theirs
        .map(|b| format_size(b.size))
        .unwrap_or_else(|| "absent".into());
    format!("ours {o}  ·  theirs {t}")
}

fn short_oid(oid: &gix::ObjectId) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn sibling_side_name_preserves_extension() {
        let p = PathBuf::from("assets/hero.psd");
        assert_eq!(
            sibling_side_name(&p, ConflictChoice::Theirs),
            PathBuf::from("assets/hero.theirs.psd")
        );
        assert_eq!(
            sibling_side_name(&p, ConflictChoice::Ours),
            PathBuf::from("assets/hero.ours.psd")
        );
    }

    #[test]
    fn sibling_side_name_handles_no_extension() {
        let p = PathBuf::from("scripts/Makefile");
        assert_eq!(
            sibling_side_name(&p, ConflictChoice::Ours),
            PathBuf::from("scripts/Makefile.ours")
        );
    }

    #[test]
    fn sibling_side_name_handles_multi_dot_extension() {
        // `archive.tar.gz` — only `.gz` is treated as the extension,
        // matching Path::extension()'s behaviour.
        let p = PathBuf::from("release/archive.tar.gz");
        assert_eq!(
            sibling_side_name(&p, ConflictChoice::Theirs),
            PathBuf::from("release/archive.tar.theirs.gz")
        );
    }

    #[test]
    fn sibling_side_name_handles_dotfile() {
        // Leading-dot name with no stem — treat the whole thing as the
        // base name, append the side tag.
        let p = PathBuf::from(".gitattributes");
        assert_eq!(
            sibling_side_name(&p, ConflictChoice::Ours),
            PathBuf::from(".gitattributes.ours")
        );
    }

    #[test]
    fn sibling_side_name_root_relative_path() {
        let p = PathBuf::from("image.png");
        assert_eq!(
            sibling_side_name(&p, ConflictChoice::Theirs),
            PathBuf::from("image.theirs.png")
        );
    }

    #[test]
    fn is_image_like_recognises_common_formats() {
        assert!(is_image_like(Path::new("hero.png")));
        assert!(is_image_like(Path::new("hero.JPG")));
        assert!(is_image_like(Path::new("layers.psd")));
        assert!(!is_image_like(Path::new("model.fbx")));
        assert!(!is_image_like(Path::new("lib.dll")));
        assert!(!is_image_like(Path::new("README")));
    }

    #[test]
    fn intents_are_equatable() {
        // `PartialEq` is load-bearing for test assertions elsewhere
        // (we compare emitted intents in integration tests that route
        // through this module).
        assert_eq!(
            BinaryConflictIntent::UseOurs,
            BinaryConflictIntent::UseOurs
        );
        assert_ne!(
            BinaryConflictIntent::UseOurs,
            BinaryConflictIntent::UseTheirs
        );
        assert_eq!(
            BinaryConflictIntent::KeepBoth {
                keep_as_main: ConflictChoice::Ours,
            },
            BinaryConflictIntent::KeepBoth {
                keep_as_main: ConflictChoice::Ours,
            }
        );
        assert_ne!(
            BinaryConflictIntent::KeepBoth {
                keep_as_main: ConflictChoice::Ours,
            },
            BinaryConflictIntent::KeepBoth {
                keep_as_main: ConflictChoice::Theirs,
            }
        );
    }
}
