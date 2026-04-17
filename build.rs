//! Build-time font subsetter.
//!
//! Walks every `.rs` file under `src/` (and a handful of explicitly listed
//! "always include" code-points), collects the union of every character that
//! could appear in mergeFox's compile-time UI strings, then emits a tiny
//! Pretendard subset containing only those glyphs.
//!
//! Why?
//! ----
//! Loading the full Pretendard (or, worse, a system CJK font like
//! AppleSDGothicNeo at ~53 MB) wastes memory because the vast majority of
//! glyphs are never drawn. A `vmmap` of the running process showed CJK font
//! data dominating MALLOC_LARGE. By generating a subset that contains
//! exactly the glyphs the chrome can render, the always-loaded "Tier A"
//! font drops from ~1.5 MB (full Pretendard) to ~50–150 KB.
//!
//! Tier B (system CJK or full Pretendard) is loaded **on demand** at
//! runtime when the user opens content with non-chrome characters
//! (commit messages, file paths, code in unfamiliar scripts, etc.).
//!
//! Limitations
//! -----------
//! * `format!("{} 개", n)` would correctly capture "개", but
//!   `format!("{}", user_provided)` cannot — runtime fallback handles those.
//! * Anything stored in non-Rust files (TOML/JSON/Markdown) is ignored
//!   here. Add explicit characters to `EXTRA_CHARS` below if needed.
//! * If `assets/fonts/Pretendard-Regular.otf` is missing the build still
//!   succeeds — we emit a 0-byte placeholder so the include_bytes! at the
//!   call site doesn't fail to compile, and `fonts.rs` skips Tier A when it
//!   sees an empty buffer. This keeps `cargo check` working in fresh clones
//!   that haven't fetched assets yet.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use syn::visit::Visit;

const FONT_PATH: &str = "assets/fonts/Pretendard-Regular.otf";
const OUT_FILE: &str = "ui-chrome.otf";

/// Characters that compile-time scanning can't or shouldn't see, but the
/// UI needs available. Examples:
///
/// * `format!` placeholders that always stay ASCII themselves but produce
///   localized literals via i18n at runtime — we scan source so this is
///   fine for our hard-coded labels, but defensive extras don't hurt.
/// * Modifier-key glyphs and pseudo-icons used in tooltips/menus.
/// * Arrow / box-drawing characters used by the diff viewer's chrome.
const EXTRA_CHARS: &str = "\
    ⌘⌥⇧⌃↵⌫↑↓←→\
    ✓✗⚠✨📝⏳🆘⟳⚙ℹ🛑\
    ─│┌┐└┘├┤┬┴┼\
    «»‹›‘’“”…\
    •·○●◯◉\
    ▾▴▸▴◀▶\
    ";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={FONT_PATH}");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/packed-refs");
    println!("cargo:rerun-if-changed=.git/refs");

    emit_build_metadata();

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR set by cargo"));
    let out_path = out_dir.join(OUT_FILE);

    // 1. Read base font. If missing, emit a placeholder and bail with a
    //    cargo warning — this lets `cargo check` work in environments
    //    without the asset (e.g. a fresh clone before LFS pull).
    let font_bytes = match fs::read(FONT_PATH) {
        Ok(b) => b,
        Err(e) => {
            println!(
                "cargo:warning=mergefox: {FONT_PATH} not found ({e}); UI chrome font disabled"
            );
            fs::write(&out_path, &[]).expect("write empty font placeholder");
            return;
        }
    };

    // 2. Collect codepoints used by the chrome.
    let mut codepoints: BTreeSet<u32> = BTreeSet::new();

    // ASCII printable + tab/newline are always needed.
    for c in 0x09u32..=0x7E {
        codepoints.insert(c);
    }

    // Latin-1 supplement + Latin Extended (covers French/Spanish accents).
    for c in 0xA0u32..=0x024F {
        codepoints.insert(c);
    }

    for c in EXTRA_CHARS.chars() {
        codepoints.insert(c as u32);
    }

    walk_rust_sources(Path::new("src"), &mut codepoints);

    // 3. Map codepoints → glyph IDs via the font's cmap.
    //    `ttf-parser` decodes the OTF/TTF tables; subsetter then keeps only
    //    the requested glyphs (plus their dependencies, e.g. composites).
    let face = ttf_parser::Face::parse(&font_bytes, 0)
        .expect("Pretendard-Regular.otf failed to parse — corrupted asset?");

    // .notdef (glyph 0) is always required by OpenType.
    let mut glyphs: BTreeSet<u16> = BTreeSet::new();
    glyphs.insert(0);

    let mut hits = 0usize;
    for cp in &codepoints {
        if let Some(c) = char::from_u32(*cp) {
            if let Some(gid) = face.glyph_index(c) {
                glyphs.insert(gid.0);
                hits += 1;
            }
        }
    }

    println!(
        "cargo:warning=mergefox font subset: {} codepoints requested, {} mapped, {} glyphs",
        codepoints.len(),
        hits,
        glyphs.len()
    );

    // 4. Run the subsetter. Output is a fully-valid OTF that contains only
    //    the kept glyphs. Pretendard is a CFF font; subsetter handles
    //    CFF/CFF2 properly. The 0.2 API takes a `GlyphRemapper` rather
    //    than a raw glyph iterator — `new_from_glyphs_sorted` builds the
    //    monotonic mapping it expects.
    let glyph_vec: Vec<u16> = glyphs.into_iter().collect();
    let remapper = subsetter::GlyphRemapper::new_from_glyphs_sorted(&glyph_vec);
    let subset_bytes = match subsetter::subset(&font_bytes, 0, &remapper) {
        Ok(b) => b,
        Err(e) => {
            println!("cargo:warning=mergefox: font subsetting failed ({e}); shipping full font");
            font_bytes.clone()
        }
    };

    fs::write(&out_path, &subset_bytes).expect("write subsetted font");

    println!(
        "cargo:warning=mergefox font subset: {} bytes (was {})",
        subset_bytes.len(),
        font_bytes.len(),
    );
}

fn emit_build_metadata() {
    let commit = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=MERGEFOX_BUILD_COMMIT={commit}");
}

/// Walk `src/` and feed every `.rs` file's string literals into the
/// codepoint set. We use `syn` rather than a regex so that string contents
/// in comments / docstrings / raw byte literals don't get misclassified.
fn walk_rust_sources(root: &Path, out: &mut BTreeSet<u32>) {
    let walker = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().map(|x| x == "rs").unwrap_or(false));

    let mut visitor = LitVisitor { out };

    for entry in walker {
        let src = match fs::read_to_string(entry.path()) {
            Ok(s) => s,
            Err(_) => continue,
        };
        // Files that fail to parse (transient typo while editing) are
        // skipped — the failed file's contributions just won't be in this
        // build's subset. Cargo will retrigger on the next valid save.
        let Ok(file) = syn::parse_file(&src) else {
            println!(
                "cargo:warning=mergefox: skipped {} (syn parse error)",
                entry.path().display()
            );
            continue;
        };
        visitor.visit_file(&file);
    }
}

struct LitVisitor<'a> {
    out: &'a mut BTreeSet<u32>,
}

impl<'ast, 'a> Visit<'ast> for LitVisitor<'a> {
    fn visit_lit_str(&mut self, lit: &'ast syn::LitStr) {
        for c in lit.value().chars() {
            self.out.insert(c as u32);
        }
    }

    fn visit_lit_char(&mut self, lit: &'ast syn::LitChar) {
        self.out.insert(lit.value() as u32);
    }
}
