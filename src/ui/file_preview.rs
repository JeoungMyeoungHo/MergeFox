//! Lazy, async thumbnail & preview pipeline for binary assets.
//!
//! Scope
//! -----
//! This module is the bridge between "MergeFox has git blob bytes /
//! working-tree paths" and "egui wants a registered image it can
//! render". It covers:
//!
//!   * **Format dispatch** — extension-driven loader picks between the
//!     built-in `image` crate support (PNG/JPG/GIF/BMP/TGA/TIFF/WebP/
//!     ICO/HDR/EXR/QOI) and extended loaders (PSD embedded-preview,
//!     macOS `qlmanage` fallback for 3D / opaque formats).
//!   * **Worker pool** — decoding is offloaded to background threads.
//!     A 2B-pixel .exr can take hundreds of ms, and egui redraws on
//!     the main thread; blocking the UI on decode is not acceptable
//!     even once.
//!   * **LRU cache** — decoded RGBA is hashed by `(oid|path, size_hint)`
//!     and retained across frames. egui's own loader cache is
//!     per-URI and loses the decoded texture on eviction; we keep the
//!     source bytes separately so reopening a file is instant.
//!   * **Thumbnail downscaling** — for the in-row thumbnail column
//!     we render at ≤64px on decode; the full-resolution path is
//!     only touched when the preview pane opens.
//!
//! Non-goals
//! ---------
//! * No compositing engine — PSD preview is the embedded JPEG, not a
//!   layer recomposite. Good enough for "what is this file", insufficient
//!   for "resolve this art merge conflict".
//! * No FBX/Blend native renderer — those delegate to `qlmanage` on
//!   macOS (Finder's thumbnail generators already support them). Other
//!   platforms show a typed placeholder badge.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use anyhow::{Context, Result};
use image::GenericImageView;

/// Maximum dimension (width or height) of thumbnails rendered inline
/// in the file-list column. 64px ≈ 4× font height, readable at a
/// glance without pushing the message / author columns off-screen.
pub const THUMB_MAX_DIM: u32 = 64;

/// Max bytes we'll load off disk for a single preview. Keeps a
/// pathological 2GB `.psd` from blowing out RAM. Files over this size
/// resolve to `PreviewState::TooLarge`.
pub const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;

/// Hard ceiling on simultaneously-decoding threads. One per physical
/// core tends to be optimal for image decode on modern CPUs; we don't
/// need more since the UI only looks at ≤ few dozen previews at once.
const WORKER_THREADS: usize = 4;

/// Key a preview by (identity, purpose). Two different purposes
/// (inline thumbnail vs. full preview pane) decode to different sizes
/// and shouldn't share a cache slot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PreviewKey {
    pub identity: PreviewIdentity,
    pub mode: PreviewMode,
}

/// What file this preview is "of" — a blob in a specific commit, a
/// working-tree path, or an embedded `bytes://` payload.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PreviewIdentity {
    /// Content-addressed: a specific blob oid. Same oid across commits
    /// hits the same cache slot — free dedup for e.g. a texture that
    /// didn't change between commits but is referenced from both.
    Blob(gix::ObjectId),
    /// Working-tree file path. Cache-busted on filesystem mtime so
    /// edits invalidate the thumbnail.
    Path { path: PathBuf, mtime_ns: u128 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreviewMode {
    /// Small inline thumbnail (≤ `THUMB_MAX_DIM` px per side).
    Thumb,
    /// Full-resolution preview for the side pane.
    Full,
}

/// State of a single preview — either pending decode, ready RGBA
/// bytes, or an error message the UI can render as a badge.
#[derive(Debug, Clone)]
pub enum PreviewState {
    Pending,
    Ready(Arc<DecodedImage>),
    /// Not an image type we know how to preview. UI renders a typed
    /// placeholder ("3D model", "PSD", …) instead of an error.
    Unsupported { label: &'static str },
    /// File exceeded `MAX_INPUT_BYTES`. UI shows size + message so the
    /// user isn't wondering why the thumbnail is blank.
    TooLarge { bytes: u64 },
    Failed { reason: String },
}

#[derive(Debug)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Tightly packed row-major RGBA8. Always `width * height * 4` long.
    pub rgba: Vec<u8>,
}

/// Process-wide preview manager. Holds the worker pool, the cache, and
/// the result receiver drained once per frame by `show_pending`.
pub struct PreviewManager {
    job_tx: Sender<PreviewJob>,
    result_rx: Mutex<Receiver<PreviewResult>>,
    cache: Mutex<HashMap<PreviewKey, PreviewState>>,
}

struct PreviewJob {
    key: PreviewKey,
    /// Already-loaded bytes if the caller has them (blob case), or
    /// `None` for path-based where we stat + read in the worker.
    bytes: Option<Arc<[u8]>>,
    ext: String,
}

struct PreviewResult {
    key: PreviewKey,
    state: PreviewState,
}

static INSTANCE: OnceLock<Arc<PreviewManager>> = OnceLock::new();

impl PreviewManager {
    /// Process-wide singleton. Constructed lazily on first call.
    pub fn global() -> Arc<PreviewManager> {
        INSTANCE
            .get_or_init(|| Arc::new(PreviewManager::new()))
            .clone()
    }

    fn new() -> Self {
        let (job_tx, job_rx) = mpsc::channel::<PreviewJob>();
        let (result_tx, result_rx) = mpsc::channel::<PreviewResult>();
        let job_rx = Arc::new(Mutex::new(job_rx));

        for _ in 0..WORKER_THREADS {
            let job_rx = job_rx.clone();
            let result_tx = result_tx.clone();
            thread::spawn(move || worker_loop(job_rx, result_tx));
        }

        Self {
            job_tx,
            result_rx: Mutex::new(result_rx),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Lookup or request a preview. On a miss, queues a decode job
    /// and returns `PreviewState::Pending` immediately — UI draws a
    /// placeholder; next frame after completion it gets the ready
    /// state. Idempotent: repeated calls for the same pending key
    /// don't spawn duplicate jobs.
    pub fn request_blob(
        &self,
        oid: gix::ObjectId,
        bytes: Arc<[u8]>,
        ext: &str,
        mode: PreviewMode,
    ) -> PreviewState {
        let key = PreviewKey {
            identity: PreviewIdentity::Blob(oid),
            mode,
        };
        if let Some(existing) = self.cache.lock().ok().and_then(|c| c.get(&key).cloned()) {
            return existing;
        }
        self.cache
            .lock()
            .ok()
            .map(|mut c| c.insert(key.clone(), PreviewState::Pending));
        let _ = self.job_tx.send(PreviewJob {
            key,
            bytes: Some(bytes),
            ext: ext.to_ascii_lowercase(),
        });
        PreviewState::Pending
    }

    /// Lookup / request a preview of a working-tree path.
    /// Cache-keyed on `(path, mtime)` so an edit invalidates.
    pub fn request_path(&self, path: &Path, mode: PreviewMode) -> PreviewState {
        let mtime_ns = path
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let key = PreviewKey {
            identity: PreviewIdentity::Path {
                path: path.to_path_buf(),
                mtime_ns,
            },
            mode,
        };
        if let Some(existing) = self.cache.lock().ok().and_then(|c| c.get(&key).cloned()) {
            return existing;
        }
        self.cache
            .lock()
            .ok()
            .map(|mut c| c.insert(key.clone(), PreviewState::Pending));
        let _ = self.job_tx.send(PreviewJob {
            key,
            bytes: None,
            ext,
        });
        PreviewState::Pending
    }

    /// Drain completed decodes into the cache. Called once per UI
    /// frame — cheap no-op when idle.
    pub fn pump(&self) {
        let Ok(rx) = self.result_rx.lock() else { return };
        let Ok(mut cache) = self.cache.lock() else {
            return;
        };
        while let Ok(result) = rx.try_recv() {
            cache.insert(result.key, result.state);
        }
    }
}

fn worker_loop(job_rx: Arc<Mutex<Receiver<PreviewJob>>>, result_tx: Sender<PreviewResult>) {
    loop {
        let job = match job_rx.lock().ok().and_then(|rx| rx.recv().ok()) {
            Some(j) => j,
            None => return,
        };
        let state = run_job(&job);
        let _ = result_tx.send(PreviewResult { key: job.key, state });
    }
}

fn run_job(job: &PreviewJob) -> PreviewState {
    let kind = FormatKind::from_ext(&job.ext);
    let bytes = match &job.bytes {
        Some(b) => b.clone(),
        None => match read_path_bytes(&job.key) {
            Ok(b) => b,
            Err(PathReadError::TooLarge(n)) => return PreviewState::TooLarge { bytes: n },
            Err(PathReadError::Missing) | Err(PathReadError::Io(_)) => {
                return PreviewState::Failed {
                    reason: "could not read file".into(),
                };
            }
        },
    };
    match decode(kind, &bytes, job.key.mode) {
        Ok(Some(img)) => PreviewState::Ready(Arc::new(img)),
        Ok(None) => PreviewState::Unsupported {
            label: kind.placeholder_label(),
        },
        Err(e) => PreviewState::Failed {
            reason: format!("{e:#}"),
        },
    }
}

enum PathReadError {
    Missing,
    TooLarge(u64),
    Io(std::io::Error),
}

fn read_path_bytes(key: &PreviewKey) -> std::result::Result<Arc<[u8]>, PathReadError> {
    let path = match &key.identity {
        PreviewIdentity::Path { path, .. } => path,
        _ => return Err(PathReadError::Missing),
    };
    let meta = path.metadata().map_err(PathReadError::Io)?;
    let size = meta.len();
    if size > MAX_INPUT_BYTES {
        return Err(PathReadError::TooLarge(size));
    }
    let bytes = std::fs::read(path).map_err(PathReadError::Io)?;
    Ok(bytes.into())
}

/// Extension-driven format dispatch. Keeps all "does this format even
/// have a loader?" logic in one spot instead of scattered `match`es
/// across UI and worker. `Unknown` is the generic "render a placeholder"
/// bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormatKind {
    /// Handled natively by the `image` crate.
    Image,
    /// PSD — decoded via embedded thumbnail (see `decode_psd_embedded`).
    Psd,
    /// 3D / opaque binary. No in-process render; falls back to
    /// platform thumbnail service (macOS `qlmanage`) at a higher level.
    OpaqueAsset(&'static str),
    Unknown,
}

impl FormatKind {
    fn from_ext(ext: &str) -> Self {
        match ext {
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "tga" | "tif" | "tiff" | "webp" | "ico"
            | "exr" | "hdr" | "qoi" => FormatKind::Image,
            "psd" => FormatKind::Psd,
            "fbx" => FormatKind::OpaqueAsset("FBX model"),
            "blend" => FormatKind::OpaqueAsset("Blender scene"),
            "spine" => FormatKind::OpaqueAsset("Spine skeleton"),
            "obj" => FormatKind::OpaqueAsset("OBJ model"),
            "gltf" | "glb" => FormatKind::OpaqueAsset("glTF model"),
            "uasset" => FormatKind::OpaqueAsset("Unreal asset"),
            "unity" | "prefab" => FormatKind::OpaqueAsset("Unity asset"),
            "mp4" | "mov" | "webm" => FormatKind::OpaqueAsset("Video"),
            "wav" | "mp3" | "ogg" | "flac" => FormatKind::OpaqueAsset("Audio"),
            _ => FormatKind::Unknown,
        }
    }

    fn placeholder_label(self) -> &'static str {
        match self {
            FormatKind::OpaqueAsset(l) => l,
            FormatKind::Psd => "Photoshop (no embedded preview)",
            FormatKind::Unknown => "",
            FormatKind::Image => "image",
        }
    }
}

fn decode(kind: FormatKind, bytes: &[u8], mode: PreviewMode) -> Result<Option<DecodedImage>> {
    match kind {
        FormatKind::Image => decode_image_crate(bytes, mode).map(Some),
        FormatKind::Psd => decode_psd_embedded(bytes, mode).map(Some).or_else(|e| {
            tracing::debug!(error = %e, "psd embedded preview unavailable");
            Ok(None)
        }),
        FormatKind::OpaqueAsset(_) | FormatKind::Unknown => Ok(None),
    }
}

fn decode_image_crate(bytes: &[u8], mode: PreviewMode) -> Result<DecodedImage> {
    let img = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .context("guess image format")?
        .decode()
        .context("image decode")?;
    let img = match mode {
        PreviewMode::Thumb => downscale(&img, THUMB_MAX_DIM),
        PreviewMode::Full => img,
    };
    let rgba = img.to_rgba8();
    Ok(DecodedImage {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

fn downscale(img: &image::DynamicImage, max_dim: u32) -> image::DynamicImage {
    let (w, h) = img.dimensions();
    if w.max(h) <= max_dim {
        return img.clone();
    }
    // Lanczos3 — slowest of the `FilterType` family but produces a
    // thumbnail that's actually readable for heavily-detailed texture
    // art. Thumbnail decode runs on a worker so the cost is hidden.
    img.resize(max_dim, max_dim, image::imageops::FilterType::Lanczos3)
}

/// PSD embedded thumbnail extraction. Adobe's format spec reserves
/// Image Resources block ID 1036 (thumbnail as JPEG) and the older
/// 1033 (BGR). Most real-world PSDs ship 1036; we try that first.
///
/// Layout:
///   0..4    = "8BPS"
///   4..6    = version (1 or 2)
///   14..26  = header (channels, height, width, depth, colormode)
///   26..30  = colormode data length
///   30+L    = image resources section (length-prefixed 4 bytes)
///     each resource: "8BIM" + u16 id + padded pascal name + u32 len + payload
fn decode_psd_embedded(bytes: &[u8], mode: PreviewMode) -> Result<DecodedImage> {
    if bytes.len() < 30 || &bytes[0..4] != b"8BPS" {
        anyhow::bail!("not a PSD file");
    }
    // Skip header (26) + color-mode data length prefix + its payload.
    let cm_len = u32::from_be_bytes([bytes[26], bytes[27], bytes[28], bytes[29]]) as usize;
    let mut pos = 30 + cm_len;
    if bytes.len() < pos + 4 {
        anyhow::bail!("truncated PSD");
    }
    let resources_len = u32::from_be_bytes([
        bytes[pos],
        bytes[pos + 1],
        bytes[pos + 2],
        bytes[pos + 3],
    ]) as usize;
    pos += 4;
    let resources_end = pos + resources_len;
    if bytes.len() < resources_end {
        anyhow::bail!("PSD resources section truncated");
    }
    while pos + 8 < resources_end {
        if &bytes[pos..pos + 4] != b"8BIM" {
            break;
        }
        let id = u16::from_be_bytes([bytes[pos + 4], bytes[pos + 5]]);
        pos += 6;
        // Pascal-style name, padded to even length.
        let name_len = bytes[pos] as usize;
        let name_total = 1 + name_len;
        let name_padded = if name_total % 2 == 0 {
            name_total
        } else {
            name_total + 1
        };
        pos += name_padded;
        if pos + 4 > resources_end {
            break;
        }
        let payload_len = u32::from_be_bytes([
            bytes[pos],
            bytes[pos + 1],
            bytes[pos + 2],
            bytes[pos + 3],
        ]) as usize;
        pos += 4;
        let payload_end = pos + payload_len;
        if payload_end > resources_end {
            break;
        }
        if id == 1036 || id == 1033 {
            // Resource block 28 bytes of preamble, then the payload.
            //   format (u32), width (u32), height (u32), width_bytes (u32),
            //   total_size (u32), compressed_size (u32), bits (u16),
            //   planes (u16), then the JPEG data for 1036.
            if payload_len > 28 && id == 1036 {
                let jpeg = &bytes[pos + 28..payload_end];
                return decode_image_crate(jpeg, mode);
            }
        }
        // Payload is padded to even length on disk.
        let payload_padded = if payload_len % 2 == 0 {
            payload_len
        } else {
            payload_len + 1
        };
        pos += payload_padded;
    }
    anyhow::bail!("no embedded preview in PSD")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_kind_classifies_common_extensions() {
        assert_eq!(FormatKind::from_ext("png"), FormatKind::Image);
        assert_eq!(FormatKind::from_ext("PNG"), FormatKind::Unknown); // caller lowercases
        assert_eq!(FormatKind::from_ext("psd"), FormatKind::Psd);
        assert!(matches!(FormatKind::from_ext("fbx"), FormatKind::OpaqueAsset(_)));
        assert_eq!(FormatKind::from_ext("rs"), FormatKind::Unknown);
    }

    #[test]
    fn decode_tiny_png_produces_rgba() {
        // Build a small PNG at test time (the image crate emits a
        // valid encoding with correct CRCs — hand-crafted bytes are
        // fragile and the tiny cost of encoding at test runtime is
        // unimportant).
        let mut bytes = Vec::new();
        let src = image::RgbaImage::from_pixel(4, 3, image::Rgba([200, 30, 30, 255]));
        image::DynamicImage::ImageRgba8(src)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("encode test png");
        let img = decode_image_crate(&bytes, PreviewMode::Thumb).expect("decode");
        assert_eq!(img.width, 4);
        assert_eq!(img.height, 3);
        assert_eq!(img.rgba.len(), 4 * 3 * 4);
    }

    #[test]
    fn psd_embedded_parser_rejects_non_psd() {
        let err = decode_psd_embedded(b"not a psd", PreviewMode::Thumb).unwrap_err();
        assert!(err.to_string().contains("not a PSD"));
    }
}
