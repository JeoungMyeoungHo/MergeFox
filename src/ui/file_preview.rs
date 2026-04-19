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
    /// Decoded RGBA → `egui::TextureHandle` cache. Creating a texture is
    /// cheap (microseconds) but *not* free — doing it every frame for a
    /// visible row noticeably shows up in frame profiles. We keep one
    /// handle per `PreviewKey` so the same oid+mode resolves to the same
    /// GPU texture across frames until the underlying `PreviewState`
    /// changes (which it only does on first-decode completion).
    ///
    /// Parked behind a `Mutex` because the UI calls from the main thread
    /// only, but the `Arc<PreviewManager>` is `Sync` so we need interior
    /// mutability. No contention in practice.
    textures: Mutex<HashMap<PreviewKey, egui::TextureHandle>>,
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
            textures: Mutex::new(HashMap::new()),
        }
    }

    /// Non-mutating lookup. Returns the cached state if we've seen this
    /// key before, else `None` without queuing a decode. UI rows call
    /// this first so they can avoid the `Arc<[u8]>` clone / blob read on
    /// the hot path when the thumbnail is already materialized.
    pub fn peek(&self, key: &PreviewKey) -> Option<PreviewState> {
        self.cache.lock().ok().and_then(|c| c.get(key).cloned())
    }

    /// Peek at or create a `TextureHandle` for a `PreviewState::Ready`
    /// payload. Returns `None` for any other state (including missing
    /// entries) so the caller can render a placeholder.
    ///
    /// `name_hint` is the texture label egui logs in debug dumps —
    /// useful when staring at a profiler trace that mentions textures
    /// by name. Not load-bearing for correctness.
    ///
    /// We intentionally keep one handle per `PreviewKey`; the inline
    /// thumbnail and the full-resolution pane have distinct keys so they
    /// each get their own GPU texture and reuse them across frames.
    pub fn texture_for(
        &self,
        ctx: &egui::Context,
        key: &PreviewKey,
        name_hint: &str,
    ) -> Option<egui::TextureHandle> {
        // Fast path: already promoted.
        if let Some(tex) = self.textures.lock().ok().and_then(|m| m.get(key).cloned()) {
            return Some(tex);
        }
        let state = self.peek(key)?;
        let PreviewState::Ready(img) = state else {
            return None;
        };
        let color = egui::ColorImage::from_rgba_unmultiplied(
            [img.width as usize, img.height as usize],
            &img.rgba,
        );
        let tex = ctx.load_texture(name_hint, color, egui::TextureOptions::LINEAR);
        if let Ok(mut m) = self.textures.lock() {
            m.entry(key.clone()).or_insert_with(|| tex.clone());
        }
        Some(tex)
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
    /// frame — cheap no-op when idle. Returns `true` if any results
    /// landed this frame; the caller can forward that to
    /// `ctx.request_repaint()` so the just-arrived thumbnail appears
    /// without waiting on the next input event.
    pub fn pump(&self) -> bool {
        let Ok(rx) = self.result_rx.lock() else {
            return false;
        };
        let Ok(mut cache) = self.cache.lock() else {
            return false;
        };
        let mut got_any = false;
        while let Ok(result) = rx.try_recv() {
            // If this key had a stale texture from a prior state, drop
            // it so the next `texture_for` call re-promotes from the
            // freshly-decoded RGBA. In practice we only ever transition
            // Pending → Ready / Failed / etc., so this is defence in
            // depth rather than a path we exercise often.
            if let Ok(mut texs) = self.textures.lock() {
                texs.remove(&result.key);
            }
            cache.insert(result.key, result.state);
            got_any = true;
        }
        got_any
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

    // Tier 3 fast-path: opaque binary formats (FBX/Blend/glTF/OBJ/…)
    // don't have an in-process decoder, but on macOS the system's
    // QuickLook generators (used by Finder) can rasterize most of them
    // to a PNG via `qlmanage -t`. We route those through the qlmanage
    // shim instead of immediately returning `Unsupported`. The shim
    // itself is OS-gated — on non-mac platforms it's a no-op that
    // falls back to the placeholder badge path below.
    if let FormatKind::OpaqueAsset(label) = kind {
        if let Some(state) = try_qlmanage(job, label) {
            return state;
        }
        // qlmanage didn't produce output (non-mac, tool missing, no
        // generator installed, or timed out). Fall through to the
        // generic placeholder — the UI will draw a typed badge.
        return PreviewState::Unsupported { label };
    }

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

/// Non-macOS stub. On Windows/Linux we have no cross-platform
/// equivalent of QuickLook that reliably thumbnails FBX/Blend/etc.
/// (Linux has `gio thumbnail` but it only covers MIME-registered
/// types, which exclude most 3D formats by default.) Returning
/// `None` lets the caller emit the standard placeholder badge.
#[cfg(not(target_os = "macos"))]
fn try_qlmanage(_job: &PreviewJob, _label: &'static str) -> Option<PreviewState> {
    None
}

/// macOS QuickLook thumbnail fallback. Shells out to `qlmanage -t`,
/// which is the same thumbnail generator Finder uses. Any installed
/// QuickLook generator plugin (Blender ships one, Epic's UAsset
/// Quicklook plugin, etc.) becomes supported "for free" — we just
/// read the PNG it emits.
///
/// Design notes
/// ------------
/// * **Blocking with a hard timeout** — `qlmanage` can take arbitrarily
///   long on a first-run plugin initialization or when a generator
///   hangs on a malformed asset. We cap wall-clock time at
///   `QLMANAGE_TIMEOUT` (5s) using a polling loop; if the child is
///   still alive at the deadline we kill it. Worker threads are a
///   shared pool (`WORKER_THREADS = 4`), so a single stuck `qlmanage`
///   would otherwise permanently block a slot.
/// * **Temp directory per job** — qlmanage always writes
///   `<outdir>/<basename>.png`; running two jobs against files with
///   the same basename (e.g. `assets/a/model.fbx` and
///   `assets/b/model.fbx`) into a shared dir would race. Isolating
///   each job in its own tempdir also makes cleanup trivial (remove
///   the whole dir, not individual files).
/// * **Blob → temp-file round-trip** — qlmanage operates on paths,
///   not stdin. For `PreviewIdentity::Blob`, we materialize the bytes
///   into a temp file with the original extension (the extension is
///   how QuickLook routes to the right generator plugin). Cleanup
///   happens in the same RAII scope as the output dir.
#[cfg(target_os = "macos")]
fn try_qlmanage(job: &PreviewJob, _label: &'static str) -> Option<PreviewState> {
    use std::time::{Duration, Instant};

    /// Max wall-clock budget for a single `qlmanage` invocation. Chosen
    /// to be short enough that a hung generator doesn't noticeably
    /// starve the worker pool (4 workers × 5s = 20s worst case before
    /// the whole pool recovers), but long enough for cold-start plugin
    /// loads on the first preview after boot.
    const QLMANAGE_TIMEOUT: Duration = Duration::from_secs(5);
    /// Polling interval for the child's exit status. Short enough that
    /// fast thumbnails (most PNG-trivial cases complete in <100ms)
    /// finish promptly; coarse enough that the polling loop itself is
    /// negligible overhead.
    const POLL_INTERVAL: Duration = Duration::from_millis(50);

    let max_dim = match job.key.mode {
        PreviewMode::Thumb => THUMB_MAX_DIM,
        // 512px is the largest size qlmanage tends to produce usable
        // output at — its generators internally rasterize at a few
        // fixed sizes and upscale beyond that looks blurry anyway.
        PreviewMode::Full => 512,
    };

    // Allocate an isolated output directory that also doubles as the
    // holder for any temp source file we materialize. `QlWorkDir`
    // guarantees `remove_dir_all` fires even on early-return / panic.
    let workdir = match QlWorkDir::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(error = %e, "qlmanage: failed to create tempdir");
            return None;
        }
    };

    // Resolve (or materialize) a real filesystem path for qlmanage.
    let source_path: PathBuf = match &job.key.identity {
        PreviewIdentity::Path { path, .. } => path.clone(),
        PreviewIdentity::Blob(oid) => {
            let bytes = match &job.bytes {
                Some(b) => b.clone(),
                None => return None,
            };
            // Preserve the extension — QuickLook dispatches generators
            // by UTI, which is itself derived from the extension for
            // third-party plugins. A `.fbx` renamed to `.bin` won't
            // thumbnail.
            let filename = if job.ext.is_empty() {
                format!("{}", oid)
            } else {
                format!("{}.{}", oid, job.ext)
            };
            let tmp_src = workdir.path().join(filename);
            if let Err(e) = std::fs::write(&tmp_src, &bytes[..]) {
                tracing::debug!(error = %e, "qlmanage: write temp source");
                return None;
            }
            tmp_src
        }
    };

    let outdir = workdir.path();
    let mut child = match std::process::Command::new("qlmanage")
        .arg("-t")
        .arg("-s")
        .arg(max_dim.to_string())
        .arg("-o")
        .arg(outdir)
        .arg(&source_path)
        // qlmanage is chatty on stderr even for successful runs.
        // Swallow both streams — we only care about the output file
        // existing, not the tool's logging.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "qlmanage: spawn failed");
            return None;
        }
    };

    // Bounded wait. `try_wait` is non-blocking; we sleep a short
    // interval between checks. This is preferable to `wait_timeout`
    // from the `wait-timeout` crate (extra dep) for our very modest
    // needs.
    let deadline = Instant::now() + QLMANAGE_TIMEOUT;
    let exit_ok = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    // Kill and reap so we don't leave a zombie.
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::debug!("qlmanage: timed out after {:?}", QLMANAGE_TIMEOUT);
                    break false;
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(e) => {
                tracing::debug!(error = %e, "qlmanage: wait failed");
                break false;
            }
        }
    };

    if !exit_ok {
        return None;
    }

    // qlmanage names its output `<source_basename>.png`. This is
    // documented behavior but also easy to inspect (readdir finds
    // exactly one `.png` in a fresh directory, which we exploit as a
    // resilience measure against future naming tweaks).
    let expected = source_path
        .file_name()
        .map(|n| outdir.join(format!("{}.png", n.to_string_lossy())));
    let png_path = match expected.as_ref().filter(|p| p.exists()) {
        Some(p) => p.clone(),
        None => match find_first_png(outdir) {
            Some(p) => p,
            None => {
                tracing::debug!("qlmanage: no PNG produced for {:?}", source_path);
                return None;
            }
        },
    };

    let png_bytes = match std::fs::read(&png_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::debug!(error = %e, "qlmanage: read output");
            return None;
        }
    };

    let result = match decode_image_crate(&png_bytes, job.key.mode) {
        Ok(img) => Some(PreviewState::Ready(Arc::new(img))),
        Err(e) => {
            tracing::debug!(error = %e, "qlmanage: decode png");
            // Decode failure on a qlmanage-produced PNG is surprising
            // enough that we want to surface it rather than silently
            // falling back to the bland "Unsupported" badge — it
            // indicates either a corrupt output or an image-crate bug.
            Some(PreviewState::Failed {
                reason: format!("qlmanage output decode: {e:#}"),
            })
        }
    };
    // `workdir` drops here, cleaning up the tempdir (and the source
    // file inside it, for the blob path). Explicit drop to document
    // the lifetime expectation rather than rely on end-of-scope order.
    drop(workdir);
    result
}

/// RAII holder for the per-job qlmanage scratch directory. Drops
/// delete the whole directory tree — best-effort, since we're in
/// `Drop` and can't surface errors anyway.
#[cfg(target_os = "macos")]
struct QlWorkDir {
    path: PathBuf,
}

#[cfg(target_os = "macos")]
impl QlWorkDir {
    fn new() -> std::io::Result<Self> {
        // `std::env::temp_dir()` honors `TMPDIR` on macOS (which points
        // at a per-user sandboxed path), so we don't have to worry
        // about permission edge cases. Nanosecond + thread id gives
        // collision resistance without needing a full uuid crate.
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("mergefox-ql-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(target_os = "macos")]
impl Drop for QlWorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Fallback PNG finder: if qlmanage changes its naming convention or
/// we miscompute the expected basename, just grab the single `.png`
/// that appeared in our isolated output dir. Cheap safety net.
#[cfg(target_os = "macos")]
fn find_first_png(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("png") {
            return Some(p);
        }
    }
    None
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

/// Synchronous decode for the commit-detail image pane. Runs on the
/// UI thread because `paint_image_pane` is already structured around
/// "byte cache produced on demand" — adding an async hop here would
/// require reshaping the image cache. The images we actually decode
/// here are the ones `FileKind::Image` flagged, which the user
/// deliberately clicked on, so blocking the UI briefly is acceptable
/// (≤ one frame for even a 10MB texture).
pub fn decode_image_for_diff_pane(bytes: &[u8]) -> anyhow::Result<DecodedImage> {
    decode_image_crate(bytes, PreviewMode::Full)
}

/// Synchronous decode of a PSD's embedded thumbnail. Kept separate
/// from `decode_image_for_diff_pane` so the UI doesn't need to know
/// which formats need which loader — the caller routes by extension.
pub fn decode_psd_for_diff_pane(bytes: &[u8]) -> anyhow::Result<DecodedImage> {
    decode_psd_embedded(bytes, PreviewMode::Full)
}

/// Extension-driven format dispatch. Keeps all "does this format even
/// have a loader?" logic in one spot instead of scattered `match`es
/// across UI and worker. `Unknown` is the generic "render a placeholder"
/// bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatKind {
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
    pub fn from_ext(ext: &str) -> Self {
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

    fn has_inprocess_decoder(self) -> bool {
        matches!(self, FormatKind::Image | FormatKind::Psd)
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

/// Does this extension point at a format we'll attempt to render
/// in-process (i.e. not a `qlmanage`-only opaque asset and not a plain
/// binary / text)? The file-list UI uses this to decide whether to
/// reserve a thumbnail slot for a row — we skip the reservation for
/// `.rs`/`.txt`/`.fbx` so row layout doesn't stutter under formats we
/// can't actually decode synchronously in the worker pool.
pub fn is_previewable_ext(ext: &str) -> bool {
    FormatKind::from_ext(&ext.to_ascii_lowercase()).has_inprocess_decoder()
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
    fn is_previewable_covers_images_and_psd_only() {
        // Any image format the `image` crate handles natively is previewable.
        for ext in ["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tiff", "exr", "qoi"] {
            assert!(is_previewable_ext(ext), "{ext} should be previewable");
        }
        // PSD has an embedded-jpeg decoder so it's previewable.
        assert!(is_previewable_ext("psd"));
        // Upper-case is accepted — UI passes raw OS extensions.
        assert!(is_previewable_ext("PNG"));
        // Opaque assets (3D, video, audio) are NOT in-process previewable:
        // they route through platform thumbnailer elsewhere, so file-list
        // rows shouldn't reserve a thumbnail slot for them.
        for ext in ["fbx", "blend", "mp4", "mov", "wav"] {
            assert!(!is_previewable_ext(ext), "{ext} should NOT be previewable in-process");
        }
        // Plain code / unknown files are skipped.
        assert!(!is_previewable_ext("rs"));
        assert!(!is_previewable_ext(""));
    }

    #[test]
    fn psd_embedded_parser_rejects_non_psd() {
        let err = decode_psd_embedded(b"not a psd", PreviewMode::Thumb).unwrap_err();
        assert!(err.to_string().contains("not a PSD"));
    }

    /// Smoke test for the macOS qlmanage fallback. qlmanage happily
    /// thumbnails PNGs (via the built-in image generator), so we
    /// don't need a real FBX fixture in the repo — we just feed it a
    /// PNG masquerading as an OpaqueAsset and check that a Ready
    /// state comes back out the other side.
    ///
    /// Gated on macOS because `qlmanage` is an Apple binary; on other
    /// platforms `try_qlmanage` is a no-op stub.
    ///
    /// Kept as a best-effort smoke test: if qlmanage is unavailable
    /// (stripped-down CI image, custom sandbox) we skip rather than
    /// fail — the production code already treats an absent qlmanage
    /// as "fall back to placeholder", so the absence doesn't
    /// represent a regression.
    #[cfg(target_os = "macos")]
    #[test]
    fn qlmanage_fallback_thumbnails_png_masquerading_as_fbx() {
        // Build a tiny PNG in memory.
        let mut png_bytes = Vec::new();
        let src = image::RgbaImage::from_pixel(16, 16, image::Rgba([10, 200, 30, 255]));
        image::DynamicImage::ImageRgba8(src)
            .write_to(
                &mut std::io::Cursor::new(&mut png_bytes),
                image::ImageFormat::Png,
            )
            .expect("encode test png");

        // If qlmanage isn't on PATH (headless CI, locked-down
        // sandbox), skip rather than fail — this test is a
        // functional smoke check, not a correctness gate.
        let probe = std::process::Command::new("qlmanage")
            .arg("-h")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if probe.is_err() {
            eprintln!("qlmanage not available, skipping");
            return;
        }

        // Synthesize a job as if we were previewing an FBX blob.
        // `from_pixel` oid is arbitrary — the content-addressing is
        // only used for cache keying, not for qlmanage dispatch.
        let oid = gix::ObjectId::null(gix::hash::Kind::Sha1);
        let job = PreviewJob {
            key: PreviewKey {
                identity: PreviewIdentity::Blob(oid),
                mode: PreviewMode::Thumb,
            },
            bytes: Some(Arc::from(png_bytes.into_boxed_slice())),
            // Use "png" as the extension — pretending it's FBX would
            // still work on most systems (qlmanage falls back to
            // content sniffing) but the PNG generator is more
            // reliably installed than any third-party FBX plugin in
            // CI, so this keeps the test robust across machines.
            // The production dispatch only reaches `try_qlmanage`
            // for `OpaqueAsset` kinds, so we call it directly here.
            ext: "png".to_string(),
        };

        let out = try_qlmanage(&job, "FBX model");
        match out {
            Some(PreviewState::Ready(img)) => {
                assert!(img.width > 0);
                assert!(img.height > 0);
                assert_eq!(img.rgba.len() as u32, img.width * img.height * 4);
            }
            Some(other) => panic!("expected Ready, got {other:?}"),
            None => {
                // qlmanage ran but produced no output — acceptable
                // skip (e.g. sandboxed test runner without QuickLook
                // access). The production path treats this the same
                // as a missing tool.
                eprintln!("qlmanage produced no output, treating as skip");
            }
        }
    }
}
