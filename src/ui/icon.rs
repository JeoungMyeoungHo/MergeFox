//! App icon for the window / dock.
//!
//! The PNG is embedded into the binary via `include_bytes!` so no runtime
//! file I/O happens — eframe calls `app_icon()` once at startup, gets RGBA
//! pixels back, and hands them to winit/Cocoa.
//!
//! Fallback: if decoding the PNG fails for any reason (corrupted asset,
//! missing feature flag), we fall back to a tiny 1×1 transparent pixel
//! rather than panicking — the window still opens, just without an icon.

use image::{GenericImageView, ImageFormat};

/// Bytes of `assets/icon.png`, embedded at compile time. The path is
/// relative to this source file.
const ICON_BYTES: &[u8] = include_bytes!("../../assets/icon.png");

pub fn app_icon() -> egui::IconData {
    match decode() {
        Ok(icon) => icon,
        Err(e) => {
            tracing::warn!(error = %e, "app icon decode failed; using placeholder");
            egui::IconData {
                rgba: vec![0, 0, 0, 0],
                width: 1,
                height: 1,
            }
        }
    }
}

fn decode() -> Result<egui::IconData, image::ImageError> {
    let img = image::load_from_memory_with_format(ICON_BYTES, ImageFormat::Png)?;
    let (width, height) = img.dimensions();
    // Convert to RGBA8 — winit / Cocoa expect pre-multiplied-straight 8-bit
    // RGBA. `to_rgba8` handles whatever colour model the PNG uses
    // internally (indexed, greyscale, RGB, etc.) and gives us a flat
    // `[R,G,B,A]` buffer.
    let rgba = img.to_rgba8().into_raw();
    Ok(egui::IconData {
        rgba,
        width,
        height,
    })
}
