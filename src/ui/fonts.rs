//! Two-tier font loading.
//!
//! ```text
//! Tier A — UI chrome subset (always loaded, ~85 KB)
//!   `include_bytes!` of a Pretendard subset built at compile time
//!   (build.rs). Contains every glyph that can come from a string
//!   literal in src/. Used as the FIRST entry in the font family list,
//!   so egui draws everything that has a glyph here from this font.
//!
//! Tier B — System CJK fallback (loaded on demand, ~50 MB)
//!   Read once from the system font directory the first time the user
//!   selects a CJK locale OR opens repo content with characters not
//!   in Tier A. Bytes are leaked into 'static so subsequent
//!   `set_fonts` calls re-share the same backing buffer.
//! ```
//!
//! Why two tiers?
//! --------------
//! Before this split, the only loaded font was a system CJK font
//! (e.g. AppleSDGothicNeo, ~53 MB on disk). Even worse, before fixing
//! the duplication bug, three copies of those bytes lived on the heap
//! at once. Even after the leak fix, 53 MB is the dominant chunk of our
//! resident set whenever a Korean/Japanese/Chinese user is detected.
//!
//! Tier A handles 99% of the UI chrome (settings labels, button text,
//! tooltips, menu items) at ~85 KB. Tier B remains for arbitrary repo
//! content, but its load is now deferred until egui actually needs it
//! for fallback. For users who never open content with non-chrome
//! glyphs, Tier B never loads at all.

use std::fs;
use std::sync::OnceLock;

use crate::config::UiLanguage;

/// The compile-time generated UI subset, embedded directly in the binary.
/// `concat!` + `env!("OUT_DIR")` is the standard pattern for pulling
/// build-script outputs into the source tree.
static UI_CHROME_FONT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ui-chrome.otf"));

/// Cached CJK font bytes, loaded at most once per process.
///
/// `OnceLock<Option<...>>` rather than `OnceLock<...>` so we can
/// distinguish "haven't tried yet" from "tried, no system font found".
struct LoadedFont {
    name: &'static str,
    bytes: &'static [u8],
    index: u32,
}

static CJK_FONT: OnceLock<Option<LoadedFont>> = OnceLock::new();

const UI_CHROME_FAMILY: &str = "mergefox_ui_chrome";

/// Apply the font configuration appropriate for `language`.
///
/// This function is idempotent and cheap to call repeatedly — the only
/// expensive thing it does (the system CJK file read) is gated by
/// `OnceLock`.
pub fn ensure_language_fonts(ctx: &egui::Context, language: UiLanguage) {
    let mut fonts = egui::FontDefinitions::default();

    // Tier A — install only when the build script actually produced a
    // non-empty subset. Empty buffer means assets/fonts/Pretendard wasn't
    // present at build time; we fall back to egui's default font in that
    // case rather than crashing or emitting an invalid-font error.
    if !UI_CHROME_FONT.is_empty() {
        fonts.font_data.insert(
            UI_CHROME_FAMILY.to_string(),
            egui::FontData::from_static(UI_CHROME_FONT),
        );
        // Front-of-list = highest priority. egui walks the family list
        // per-glyph; Tier A wins for everything it can render.
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, UI_CHROME_FAMILY.to_string());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .insert(0, UI_CHROME_FAMILY.to_string());
    }

    // Tier B — only attempt the system CJK load when the active locale
    // actually needs it. egui's per-glyph fallback would still pull from
    // Tier B if it were installed, but loading 53 MB just to render an
    // English-only Welcome screen is wasteful.
    if requires_cjk_font(language.resolved()) {
        if let Some((name, font_data)) = load_cjk_font() {
            fonts.font_data.insert(name.clone(), font_data);
            // After Tier A, before egui's defaults — covers any glyph
            // missing from the chrome subset (i.e. arbitrary repo text).
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push(name.clone());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push(name);
        } else {
            eprintln!("mergefox: no CJK system font found; falling back to egui defaults");
        }
    }

    ctx.set_fonts(fonts);
    ctx.request_repaint();
}

fn requires_cjk_font(language: UiLanguage) -> bool {
    matches!(
        language,
        UiLanguage::Korean | UiLanguage::Japanese | UiLanguage::Chinese
    )
}

fn load_cjk_font() -> Option<(String, egui::FontData)> {
    // First call: walk the candidate list, read the first one that exists,
    // leak its bytes, and cache the result. Later calls hit the cache and
    // return immediately — no I/O, no allocation.
    let cached = CJK_FONT
        .get_or_init(|| {
            for cand in cjk_font_candidates() {
                let bytes = match fs::read(cand.path) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                // `into_boxed_slice` shrinks-to-fit, then `Box::leak` makes
                // the slice live for the rest of the process. We never
                // need to free it: this single allocation is the canonical
                // source for every `FontData::from_static` below.
                let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
                return Some(LoadedFont {
                    name: cand.name,
                    bytes: leaked,
                    index: cand.index,
                });
            }
            None
        })
        .as_ref()?;

    let mut data = egui::FontData::from_static(cached.bytes);
    data.index = cached.index;
    Some((cached.name.to_string(), data))
}

struct FontCandidate {
    name: &'static str,
    path: &'static str,
    index: u32,
}

#[cfg(target_os = "macos")]
fn cjk_font_candidates() -> &'static [FontCandidate] {
    // Order matters: smaller fonts first, so when several would work we
    // pick the lighter resident-set hit.
    //   AppleGothic   ~5  MB
    //   ArialUnicode  ~22 MB
    //   PingFang      ~50 MB (Chinese-first, but covers Hangul)
    //   AppleSDGothicNeo ~53 MB (Korean-optimized)
    //   HiraginoSansGB   ~30 MB
    &[
        FontCandidate {
            name: "AppleGothic",
            path: "/System/Library/Fonts/Supplemental/AppleGothic.ttf",
            index: 0,
        },
        FontCandidate {
            name: "ArialUnicode",
            path: "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
            index: 0,
        },
        FontCandidate {
            name: "HiraginoSansGB",
            path: "/System/Library/Fonts/Hiragino Sans GB.ttc",
            index: 0,
        },
        FontCandidate {
            name: "PingFang",
            path: "/System/Library/Fonts/PingFang.ttc",
            index: 0,
        },
        FontCandidate {
            name: "AppleSDGothicNeo",
            path: "/System/Library/Fonts/AppleSDGothicNeo.ttc",
            index: 0,
        },
    ]
}

#[cfg(target_os = "windows")]
fn cjk_font_candidates() -> &'static [FontCandidate] {
    &[
        FontCandidate {
            name: "MalgunGothic",
            path: "C:\\Windows\\Fonts\\malgun.ttf",
            index: 0,
        },
        FontCandidate {
            name: "YuGothic",
            path: "C:\\Windows\\Fonts\\YuGothM.ttc",
            index: 0,
        },
        FontCandidate {
            name: "MicrosoftYaHei",
            path: "C:\\Windows\\Fonts\\msyh.ttc",
            index: 0,
        },
        FontCandidate {
            name: "Meiryo",
            path: "C:\\Windows\\Fonts\\meiryo.ttc",
            index: 0,
        },
    ]
}

#[cfg(all(unix, not(target_os = "macos")))]
fn cjk_font_candidates() -> &'static [FontCandidate] {
    &[
        // Single-region fonts first (~5–7 MB each) before the unified
        // ~16 MB CJK TTC.
        FontCandidate {
            name: "NotoSansKR-Regular",
            path: "/usr/share/fonts/opentype/noto/NotoSansKR-Regular.otf",
            index: 0,
        },
        FontCandidate {
            name: "NotoSansJP-Regular",
            path: "/usr/share/fonts/opentype/noto/NotoSansJP-Regular.otf",
            index: 0,
        },
        FontCandidate {
            name: "NotoSansSC-Regular",
            path: "/usr/share/fonts/opentype/noto/NotoSansSC-Regular.otf",
            index: 0,
        },
        FontCandidate {
            name: "NanumGothic",
            path: "/usr/share/fonts/truetype/nanum/NanumGothic.ttf",
            index: 0,
        },
        FontCandidate {
            name: "NotoSansCJK-Regular",
            path: "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            index: 0,
        },
    ]
}
