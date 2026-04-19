//! Background git clone with progress reporting.
//!
//! Runs `git clone` on a dedicated thread so the UI stays
//! responsive. Progress is shared via `Arc<Mutex<>>` and the final result
//! comes back through an `mpsc` channel.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};

#[derive(Debug, Default, Clone)]
pub struct CloneProgress {
    pub received_objects: usize,
    pub total_objects: usize,
    pub received_bytes: usize,
    pub stage: Stage,
}

/// Object-level server filter applied to `git clone`.
///
/// Why this exists
/// ---------------
/// Monorepos and game / asset repositories routinely push past the point
/// where a full clone is feasible on a developer laptop — multi-GB
/// history plus large binary blobs can easily run to tens of gigabytes.
/// Git answers this with the partial-clone machinery: the server keeps
/// everything, the client negotiates which objects it actually wants
/// up-front, and the rest are lazily fetched on demand the first time
/// they are dereferenced (via a promisor remote).
///
/// The three non-default variants map 1:1 onto the flag the system
/// `git` CLI accepts:
///
///   * `BlobNone`        → `--filter=blob:none`        (fetch tree/commit
///                                                     objects only)
///   * `BlobLimit { n }` → `--filter=blob:limit=<n>`   (skip blobs ≥ n
///                                                     bytes)
///   * `TreeZero`        → `--filter=tree:0`           (fetch only the
///                                                     root tree)
///
/// `TreeZero` in particular is only useful paired with
/// `--sparse --no-checkout` and a follow-up `git sparse-checkout set`
/// — otherwise the working tree checkout immediately dereferences every
/// tree it can see and defeats the filter. The UI enforces that pairing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CloneFilter {
    /// Full clone — every object. Default behaviour, no filter flag.
    #[default]
    None,
    /// `--filter=blob:none` — fetch blobs on demand.
    BlobNone,
    /// `--filter=blob:limit=<N>` — fetch blobs smaller than `bytes`
    /// bytes eagerly, the rest on demand.
    BlobLimit { bytes: u64 },
    /// `--filter=tree:0` — fetch only the root tree; paired with
    /// `--sparse --no-checkout`, we populate sparse-checkout patterns
    /// and then check out just those directories.
    TreeZero,
}

impl CloneFilter {
    /// True when this filter is the default (no-op) variant. Callers
    /// use this to branch between the gix-first dispatch and the CLI
    /// forced path — any non-`None` filter requires the CLI because
    /// gix does not yet plumb `--filter=…` through cleanly.
    pub fn is_enabled(&self) -> bool {
        !matches!(self, CloneFilter::None)
    }

    /// Short human label, used in the post-clone banner detail line.
    pub fn display_label(&self) -> String {
        match self {
            CloneFilter::None => "full".to_string(),
            CloneFilter::BlobNone => "blob:none".to_string(),
            CloneFilter::BlobLimit { bytes } => format!("blob:limit={}", format_bytes(*bytes)),
            CloneFilter::TreeZero => "tree:0 + sparse".to_string(),
        }
    }
}

fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{} MB", n / MB)
    } else if n >= KB {
        format!("{} KB", n / KB)
    } else {
        format!("{} B", n)
    }
}

/// Everything the clone worker needs to know about *how* to clone,
/// beyond the URL and destination.
///
/// Why a struct, not more positional args
/// --------------------------------------
/// Partial-clone support grew the clone surface from "depth yes/no" to
/// a three-axis space (depth × filter × sparse directories) where any
/// combination is valid — `--depth=1 --filter=blob:none` is a common
/// "CI checkout" pattern, `--filter=tree:0` demands a sparse set, and
/// regular shallow clones still need to work. Bundling the knobs into a
/// single `CloneOpts` keeps the `spawn_with_opts` signature stable even
/// as we add more options (e.g. protocol v2 toggle, reference list
/// pruning) in later sprints.
#[derive(Debug, Clone, Default)]
pub struct CloneOpts {
    /// `Some(n)` → `--depth=n`. None → full history. Can be combined
    /// with any `filter` — shallow + partial is a legitimate "CI style"
    /// pattern and we don't second-guess it.
    pub depth: Option<u32>,
    pub filter: CloneFilter,
    /// Sparse-checkout directory patterns in cone mode
    /// (`git sparse-checkout init --cone` then `… set <dirs…>`).
    ///
    /// Required when `filter == TreeZero` — a `tree:0` clone with no
    /// sparse set has nothing to check out and leaves the working tree
    /// empty, which is almost never what the user wanted. Optional
    /// otherwise: a non-`TreeZero` clone with sparse patterns is a
    /// valid "large checkout but full history" path (think monorepo
    /// where you only work in one product directory).
    ///
    /// Empty vec = check out everything.
    pub sparse_dirs: Vec<String>,
}

/// Build the CLI argument vector for `git clone` from a `CloneOpts`.
///
/// Split out from `do_clone` so it can be unit-tested exhaustively
/// against every `CloneFilter` variant (and combinations with `depth`)
/// without mocking `std::process::Command`. The returned vector is the
/// *flag* portion only — the caller appends `--`, the URL, and the
/// destination.
///
/// Ordering note: `--progress` is emitted first so our stderr parser
/// sees the percentage output reliably across git versions; the filter
/// / sparse flags come next (position doesn't matter to git but grouped
/// output is easier to debug in logs).
pub fn render_clone_cli_flags(opts: &CloneOpts) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    args.push("clone".to_string());
    args.push("--progress".to_string());
    if let Some(n) = opts.depth {
        args.push(format!("--depth={n}"));
    }
    match &opts.filter {
        CloneFilter::None => {}
        CloneFilter::BlobNone => args.push("--filter=blob:none".to_string()),
        CloneFilter::BlobLimit { bytes } => args.push(format!("--filter=blob:limit={bytes}")),
        CloneFilter::TreeZero => {
            args.push("--filter=tree:0".to_string());
            // `tree:0` without `--sparse --no-checkout` would pull
            // down every tree during the initial checkout and make the
            // filter pointless. The UI guarantees `sparse_dirs` is
            // non-empty in this arm, but we emit the flags regardless
            // so the post-clone sparse setup has an empty working tree
            // to populate.
            args.push("--sparse".to_string());
            args.push("--no-checkout".to_string());
        }
    }
    args
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    #[default]
    Connecting,
    Receiving,
    Resolving,
    Checkout,
}

pub struct CloneHandle {
    pub url: String,
    pub dest: PathBuf,
    /// `None` = full clone (all history). `Some(n)` = `git clone --depth n`
    /// — useful for multi-GB repos where the user opted into a shallow
    /// download via the welcome-screen size prompt. `u32` is plenty: even
    /// depth 1_000_000 is below `i32::MAX`, and depths over a few thousand
    /// defeat the point of going shallow.
    pub depth: Option<u32>,
    pub progress: Arc<Mutex<CloneProgress>>,
    pub rx: Receiver<Result<CloneOutcome, String>>,
}

/// What a completed clone tells the caller.
///
/// `account_slug` is `Some` when the background probe picked a
/// connected provider account to authenticate with — the caller stores
/// it in `RepoSettings` so subsequent push / pull on this repo defaults
/// to the same account without re-asking.
#[derive(Debug, Clone)]
pub struct CloneOutcome {
    pub path: PathBuf,
    pub account_slug: Option<String>,
    /// Which filter the worker actually applied. Copied straight from
    /// `CloneOpts::filter` — surfaced on `CloneOutcome` so the UI layer
    /// can emit the "Partial clone succeeded (blob:none, 42 MB)" banner
    /// without having to remember what it asked for.
    pub filter: CloneFilter,
    /// Sparse-checkout directories the worker applied after clone. Empty
    /// when the clone was not sparse.
    pub sparse_dirs: Vec<String>,
    /// Post-clone `git count-objects -v` `size-pack` + `size` sum, in
    /// bytes. Best-effort: `None` when the count command failed or the
    /// output didn't parse. Drives the "X MB downloaded" line in the
    /// success banner — purely informational.
    pub downloaded_bytes: Option<u64>,
}

impl CloneHandle {
    pub fn poll(&self) -> Option<Result<CloneOutcome, String>> {
        self.rx.try_recv().ok()
    }

    pub fn snapshot(&self) -> CloneProgress {
        self.progress.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

/// Back-compat shim preserved for the three or four call sites that
/// only care about `depth`. New call sites should prefer
/// `spawn_with_opts` so they can opt into partial clone / sparse
/// checkout without the signature churn.
pub fn spawn(
    url: String,
    dest: PathBuf,
    depth: Option<u32>,
    accounts: Vec<crate::providers::ProviderAccount>,
) -> CloneHandle {
    spawn_with_opts(
        url,
        dest,
        CloneOpts {
            depth,
            ..CloneOpts::default()
        },
        accounts,
    )
}

/// Clone `url` into `dest` with the caller-supplied options.
///
/// Semantics beyond what `CloneOpts` documents:
///
///   * When `opts.filter != None` or `!opts.sparse_dirs.is_empty()` we
///     force the system `git` CLI path (see `do_clone_dispatched`).
///   * If sparse-checkout post-setup fails, the cloned repository is
///     *left on disk* — the on-the-wire bytes are already paid for and
///     the user can fix the sparse patterns manually. The error still
///     propagates so the UI surfaces a toast.
pub fn spawn_with_opts(
    url: String,
    dest: PathBuf,
    opts: CloneOpts,
    accounts: Vec<crate::providers::ProviderAccount>,
) -> CloneHandle {
    let progress = Arc::new(Mutex::new(CloneProgress::default()));
    let (tx, rx) = mpsc::channel();

    let url_thread = url.clone();
    let dest_thread = dest.clone();
    let progress_thread = progress.clone();
    let opts_thread = opts.clone();
    let opts_outcome = opts.clone();

    thread::spawn(move || {
        // Probe connected accounts for a PAT that authenticates against
        // this URL. The winner's token is embedded in `fetch_url` so
        // the one-shot `git clone` / `gix` pull below runs as that
        // account; on success we rewrite `origin` to the clean URL so
        // no credential lands in `.git/config`.
        let authed = crate::clone_auth::probe(&url_thread, &accounts);
        let (fetch_url, account_slug) = match authed.as_ref() {
            Some(a) => (a.authed_url.clone(), Some(a.account.slug())),
            None => (url_thread.clone(), None),
        };

        let result = (|| -> Result<()> {
            do_clone_dispatched(&fetch_url, &dest_thread, &opts_thread, progress_thread)?;
            if authed.is_some() {
                // Scrub the PAT out of the persisted remote URL. A
                // failure here is noisy in the log but not fatal — the
                // clone itself succeeded.
                if let Err(e) = crate::git::cli::run(
                    &dest_thread,
                    ["remote", "set-url", "origin", url_thread.as_str()],
                ) {
                    tracing::warn!(error = %format!("{e:#}"), "failed to scrub token from origin URL");
                }
            }
            Ok(())
        })()
        .map(|_| {
            // Best-effort post-clone byte measurement — fed into the
            // "Clone complete: X MB downloaded" banner. A failure here
            // downgrades gracefully to "no number", it does NOT fail
            // the clone.
            let downloaded_bytes = count_repo_bytes(&dest_thread);
            CloneOutcome {
                path: dest_thread.clone(),
                account_slug,
                filter: opts_outcome.filter.clone(),
                sparse_dirs: opts_outcome.sparse_dirs.clone(),
                downloaded_bytes,
            }
        })
        .map_err(|e| format!("{e:#}"));
        let _ = tx.send(result);
    });

    CloneHandle {
        url,
        dest,
        depth: opts.depth,
        progress,
        rx,
    }
}

/// Try gitoxide first, fall back to the system `git clone` CLI on error.
///
/// Motivation
/// ----------
/// gitoxide has parallel pack resolve and a correct protocol-v2 shallow
/// implementation, both of which matter on kernel-scale repos. The
/// system `git` binary stays in the tree as an emergency fallback for
/// hosts / auth modes gix doesn't yet handle (private SSH with exotic
/// key types, some Azure DevOps quirks, etc.) so a single bad provider
/// doesn't take the whole clone flow offline.
///
/// Destination preparation (PartialClone detection, parent mkdir) runs
/// once here; each backend gets a clean destination.
fn do_clone_dispatched(
    url: &str,
    dest: &Path,
    opts: &CloneOpts,
    progress: Arc<Mutex<CloneProgress>>,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create clone parent {}", parent.display()))?;
    }

    // gix's clone path does not yet thread `--filter=…` / sparse-checkout
    // through reliably — at the time of writing, the protocol-v2 filter
    // negotiation is stubbed and the post-clone sparse steps are absent
    // entirely. Attempting partial / sparse via gix would silently
    // degrade to a full clone. Force the system `git` CLI path whenever
    // the user asked for a filter or sparse pattern set; we can relax
    // this once gix grows real support.
    let force_cli = opts.filter.is_enabled() || !opts.sparse_dirs.is_empty();

    // Classify destination once; both backends need the same sanity
    // (don't overwrite a live repo, auto-clean failed-clone stubs, etc.).
    match classify_destination(dest).with_context(|| format!("inspect {}", dest.display()))? {
        DestinationState::Empty => {}
        DestinationState::PartialClone => {
            std::fs::remove_dir_all(dest).with_context(|| {
                format!("remove partial clone at {} before retrying", dest.display())
            })?;
        }
        DestinationState::ExistingRepo => {
            anyhow::bail!(
                "a git repository already exists at {}; open it from Recents or the welcome input instead",
                dest.display()
            );
        }
        DestinationState::NonEmptyNonGit => {
            anyhow::bail!(
                "{} is not empty and doesn't look like a previous clone; choose a different target path",
                dest.display()
            );
        }
    }

    // Attempt gix first — unless the caller asked for anything gix
    // can't honour (partial clone / sparse checkout), in which case we
    // skip straight to the CLI. Forcing CLI is safer than letting gix
    // silently produce a full clone when the user explicitly opted
    // into a filter.
    let gix_err = if force_cli {
        "skipped: partial clone / sparse checkout routes through git CLI".to_string()
    } else {
        match crate::gix_clone::do_clone_gix(url, dest, opts.depth, progress.clone()) {
            Ok(()) => return Ok(()),
            Err(e) => format!("{e:#}"),
        }
    };

    // gix failed (or was skipped). Log and fall back to the `git` CLI
    // — but re-classify the destination because gix may have written a
    // partial skeleton before erroring out.
    if !force_cli {
        tracing::warn!(error = %gix_err, "gix clone failed; falling back to git CLI");
    }
    if dest.exists() {
        match classify_destination(dest)
            .with_context(|| format!("re-inspect {} after gix failure", dest.display()))?
        {
            DestinationState::PartialClone => {
                std::fs::remove_dir_all(dest).with_context(|| {
                    format!(
                        "remove partial clone at {} before git CLI retry",
                        dest.display()
                    )
                })?;
            }
            DestinationState::Empty => {}
            // If gix somehow produced a valid repo before erroring (very
            // unlikely), don't clobber it — surface the original gix error.
            DestinationState::ExistingRepo => {
                anyhow::bail!(
                    "gix clone failed ({gix_err}) and left a repository at {}; \
                     inspect manually before retrying",
                    dest.display()
                );
            }
            DestinationState::NonEmptyNonGit => {
                anyhow::bail!(
                    "gix clone failed ({gix_err}) and left files at {}; \
                     clean up before retrying",
                    dest.display()
                );
            }
        }
    }

    match do_clone(url, dest, opts, progress) {
        // git CLI fallback — see `do_clone` below.
        Ok(()) => Ok(()),
        Err(cli_err) => {
            if force_cli {
                // No gix attempt happened — don't confuse the user with
                // a bogus "both backends failed" framing when only one
                // was tried.
                anyhow::bail!("git clone failed for {url}: {cli_err:#}")
            } else {
                anyhow::bail!(
                    "both git backends failed to clone {url}:\n  - gix: {gix_err}\n  - git CLI: {cli_err:#}"
                )
            }
        }
    }
}

fn do_clone(
    url: &str,
    dest: &Path,
    opts: &CloneOpts,
    progress: Arc<Mutex<CloneProgress>>,
) -> Result<()> {
    // Fallback clone via the system `git` binary.
    //
    // Rationale: mirrors the CLI credential helper / SSH agent / proxy
    // behaviour the user already has configured. Progress is captured
    // by parsing the stderr "--progress" stream on a best-effort basis.
    if let Ok(mut p) = progress.lock() {
        p.stage = Stage::Receiving;
        p.received_objects = 0;
        p.total_objects = 0;
        p.received_bytes = 0;
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create clone parent {}", parent.display()))?;
    }

    // The flag list is built by `render_clone_cli_flags` so the exact
    // mapping from `CloneOpts` to argv is unit-tested in isolation.
    let flags = render_clone_cli_flags(opts);
    let mut cmd = std::process::Command::new("git");
    for f in &flags {
        cmd.arg(f);
    }
    cmd.arg("--").arg(url).arg(dest);

    let out = cmd
        .output()
        .with_context(|| format!("spawn git clone {url}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "git clone exited with code {}: {}",
            out.status.code().unwrap_or(-1),
            stderr.trim()
        );
    }
    if let Ok(mut p) = progress.lock() {
        p.stage = Stage::Checkout;
    }

    // Sparse-checkout post-setup.
    //
    // We intentionally run this *after* `git clone` rather than trying
    // to bake the patterns into the initial clone invocation — git's
    // `--sparse` flag alone sets up a "top-level files only" pattern,
    // which is not what the user typed in the textarea. The right
    // sequence is:
    //
    //   1. `git clone --sparse --no-checkout --filter=tree:0 …`
    //   2. `git sparse-checkout init --cone`
    //   3. `git sparse-checkout set <dir1> <dir2> …`
    //   4. `git checkout` (populates the working tree under the pattern)
    //
    // Step 1 is handled above. Steps 2–4 run here whenever the caller
    // supplied sparse_dirs — even when the filter is something other
    // than `TreeZero`, because "full history, narrow working tree" is
    // a valid combination for large monorepos.
    if !opts.sparse_dirs.is_empty() {
        configure_sparse_checkout(dest, &opts.sparse_dirs)?;
    }
    Ok(())
}

/// Drive `git sparse-checkout` to restrict the working tree to the
/// caller's directory list.
///
/// Failure policy
/// --------------
/// On error the cloned repository is **not** removed. The blob fetch
/// that just happened can be many gigabytes on the target repos this
/// feature is aimed at; auto-deleting would force the user to pay that
/// cost again after a transient filesystem error or a typo in a
/// pattern. We surface the error so the UI can toast it and let the
/// user run `git sparse-checkout set` manually to recover.
fn configure_sparse_checkout(dest: &Path, sparse_dirs: &[String]) -> Result<()> {
    crate::git::cli::run(dest, ["sparse-checkout", "init", "--cone"])
        .with_context(|| format!("git sparse-checkout init at {}", dest.display()))?;

    // `sparse-checkout set` takes the directory list as trailing
    // positional args. Cone mode rejects glob-shaped entries, so we
    // pass the strings through unchanged and let git validate them —
    // the error message from git is clearer than anything we'd
    // synthesise.
    let mut args: Vec<String> = vec!["sparse-checkout".to_string(), "set".to_string()];
    args.extend(sparse_dirs.iter().cloned());
    crate::git::cli::run(dest, args.iter().map(String::as_str))
        .with_context(|| format!("git sparse-checkout set at {}", dest.display()))?;

    // Finally, populate the working tree. `clone --no-checkout` left
    // it empty; this final checkout honours the sparse pattern we
    // just installed.
    crate::git::cli::run(dest, ["checkout"])
        .with_context(|| format!("git checkout after sparse setup at {}", dest.display()))?;
    Ok(())
}

/// Best-effort disk-size estimate for the freshly-cloned repo.
///
/// We call `git count-objects -v` and sum the reported `size` (loose
/// objects, KB) and `size-pack` (packed objects, KB) fields. This is
/// the closest approximation git exposes to "bytes on the wire" — the
/// true transfer size includes protocol overhead we can't see, but for
/// a banner labelled "downloaded" the packed size is accurate to
/// within a few percent.
///
/// Returns `None` on any failure so callers can degrade to "no
/// bandwidth line" instead of showing a nonsense number.
fn count_repo_bytes(dest: &Path) -> Option<u64> {
    let out = crate::git::cli::run(dest, ["count-objects", "-v"]).ok()?;
    let text = out.stdout_str();
    let mut size_kb: u64 = 0;
    let mut size_pack_kb: u64 = 0;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("size: ") {
            size_kb = rest.trim().parse::<u64>().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("size-pack: ") {
            size_pack_kb = rest.trim().parse::<u64>().unwrap_or(0);
        }
    }
    let total_kb = size_kb.saturating_add(size_pack_kb);
    Some(total_kb.saturating_mul(1024))
}

/// What we found at the prospective clone destination.
///
/// The classification distinguishes "retrying after a failed clone"
/// (PartialClone — safe to auto-clean) from "user picked a path that
/// already holds something valuable" (ExistingRepo or NonEmptyNonGit —
/// never auto-clean, surface a typed error so the caller can prompt for
/// a different path or open the existing repo).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestinationState {
    /// Directory doesn't exist yet, or exists and is empty.
    Empty,
    /// Directory contains `.git/` (and nothing else) but gix can't
    /// resolve HEAD — the tell-tale shape of a crashed / aborted clone.
    /// Safe to remove and retry.
    PartialClone,
    /// Directory contains a fully-formed git repository. Do NOT delete.
    ExistingRepo,
    /// Directory has real files but no `.git/`. Could be a user's work
    /// in progress, a submodule checkout, anything — do NOT touch.
    NonEmptyNonGit,
}

fn classify_destination(dest: &Path) -> Result<DestinationState> {
    if !dest.exists() {
        return Ok(DestinationState::Empty);
    }

    // Must be a directory for any of these states to make sense; a file
    // at `dest` is unambiguously "wrong", handled by the NonEmptyNonGit
    // arm (git's own error message is clearer than ours if we tried to
    // be clever here).
    if !dest.is_dir() {
        return Ok(DestinationState::NonEmptyNonGit);
    }

    let mut has_git_dir = false;
    let mut non_git_entries: usize = 0;
    for entry in fs::read_dir(dest).with_context(|| format!("read_dir {}", dest.display()))? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            has_git_dir = true;
        } else {
            non_git_entries += 1;
        }
    }

    if !has_git_dir && non_git_entries == 0 {
        return Ok(DestinationState::Empty);
    }
    if !has_git_dir {
        return Ok(DestinationState::NonEmptyNonGit);
    }

    // `.git/` is present. Try to open via gix; if it can resolve HEAD
    // it's a real repo. If open succeeds but HEAD doesn't resolve AND
    // there are no tracked files on disk, treat as partial clone (the
    // shape a crashed clone leaves behind when fetch fails mid-way).
    match gix::open(dest) {
        Ok(repo) => match repo.head_id() {
            Ok(_) => Ok(DestinationState::ExistingRepo),
            Err(_) if non_git_entries > 0 => Ok(DestinationState::ExistingRepo),
            Err(_) => Ok(DestinationState::PartialClone),
        },
        Err(_) if non_git_entries == 0 => Ok(DestinationState::PartialClone),
        Err(_) => Ok(DestinationState::ExistingRepo),
    }
}

// ---- Pre-clone size probe --------------------------------------------------
//
// Before we hand off to gix / git we try to learn the repository's size from
// its hosting provider's REST API so we can warn on multi-GB clones. The
// git wire protocol doesn't expose this up front (you'd only learn the
// size by starting the clone and measuring), so we work around it with a
// cheap provider-specific metadata call.
//
// Scope intentionally narrow:
//   * GitHub.com — `GET /repos/{o}/{r}` returns `size` in KB
//   * GitLab.com — `GET /api/v4/projects/{o}%2F{r}?statistics=true`
//     returns `statistics.repository_size` in bytes
//
// Self-hosted GitLab / Gitea / Bitbucket and every other provider return
// `Unknown` here, in which case the welcome flow proceeds directly to the
// clone without a size prompt — no wait, no noise.

pub struct ClonePreflightHandle {
    pub url: String,
    pub dest: PathBuf,
    rx: Receiver<PreflightOutcome>,
}

#[derive(Debug, Clone)]
pub enum PreflightOutcome {
    /// Provider told us the repo size (bytes on disk, approximate).
    KnownSize { bytes: u64 },
    /// Not a known provider, API failed, or timed out. Caller should
    /// fall back to "proceed without prompt".
    Unknown,
}

impl ClonePreflightHandle {
    pub fn poll(&self) -> Option<PreflightOutcome> {
        self.rx.try_recv().ok()
    }
}

/// Spawn a background probe of the repo's size. Takes the parsed
/// components so we don't re-parse on the worker thread.
pub fn spawn_preflight(
    url: String,
    dest: PathBuf,
    host: String,
    owner: String,
    repo: String,
) -> ClonePreflightHandle {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let outcome =
            crate::ai::runtime::block_on(async move { probe_size(&host, &owner, &repo).await });
        let _ = tx.send(outcome);
    });
    ClonePreflightHandle { url, dest, rx }
}

async fn probe_size(host: &str, owner: &str, repo: &str) -> PreflightOutcome {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .user_agent("mergefox")
        .build()
    {
        Ok(c) => c,
        Err(_) => return PreflightOutcome::Unknown,
    };

    match host {
        "github.com" => probe_github(&client, owner, repo).await,
        "gitlab.com" => probe_gitlab(&client, owner, repo).await,
        _ => PreflightOutcome::Unknown,
    }
}

async fn probe_github(client: &reqwest::Client, owner: &str, repo: &str) -> PreflightOutcome {
    let url = format!("https://api.github.com/repos/{owner}/{repo}");
    let resp = match client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return PreflightOutcome::Unknown,
    };
    if !resp.status().is_success() {
        return PreflightOutcome::Unknown;
    }
    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(_) => return PreflightOutcome::Unknown,
    };
    // GitHub reports `size` in kilobytes (historically KiB).
    match json.get("size").and_then(|v| v.as_u64()) {
        Some(kb) => PreflightOutcome::KnownSize {
            bytes: kb.saturating_mul(1024),
        },
        None => PreflightOutcome::Unknown,
    }
}

async fn probe_gitlab(client: &reqwest::Client, owner: &str, repo: &str) -> PreflightOutcome {
    // URL-encode the `owner/repo` slug as a single path segment — GitLab's
    // project lookup takes the escaped full path in lieu of a numeric id.
    let slug = format!("{owner}%2F{repo}");
    let url = format!("https://gitlab.com/api/v4/projects/{slug}?statistics=true");
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(_) => return PreflightOutcome::Unknown,
    };
    if !resp.status().is_success() {
        return PreflightOutcome::Unknown;
    }
    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(_) => return PreflightOutcome::Unknown,
    };
    match json
        .get("statistics")
        .and_then(|s| s.get("repository_size"))
        .and_then(|v| v.as_u64())
    {
        Some(bytes) => PreflightOutcome::KnownSize { bytes },
        None => PreflightOutcome::Unknown,
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the partial-clone / sparse-checkout plumbing.
    //!
    //! The pure argv builder `render_clone_cli_flags` is cheap to test
    //! exhaustively, so we do — every `CloneFilter` variant is covered,
    //! both with and without a depth. The two integration tests run an
    //! actual `git clone` against a local file-URL repo; they are
    //! `#[ignore]`d by default because they depend on a working `git`
    //! binary (and, for the `tree:0` test, a git new enough to support
    //! `sparse-checkout init --cone` — 2.25+). Run with
    //! `cargo test --bin mergefox -- --ignored` to exercise them.
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    #[test]
    fn render_flags_for_full_clone_is_minimal() {
        let opts = CloneOpts::default();
        assert_eq!(
            render_clone_cli_flags(&opts),
            vec!["clone".to_string(), "--progress".to_string()]
        );
    }

    #[test]
    fn render_flags_for_shallow_only() {
        let opts = CloneOpts {
            depth: Some(1),
            ..CloneOpts::default()
        };
        assert_eq!(
            render_clone_cli_flags(&opts),
            vec![
                "clone".to_string(),
                "--progress".to_string(),
                "--depth=1".to_string()
            ]
        );
    }

    #[test]
    fn render_flags_for_blob_none() {
        let opts = CloneOpts {
            filter: CloneFilter::BlobNone,
            ..CloneOpts::default()
        };
        assert_eq!(
            render_clone_cli_flags(&opts),
            vec![
                "clone".to_string(),
                "--progress".to_string(),
                "--filter=blob:none".to_string()
            ]
        );
    }

    #[test]
    fn render_flags_for_blob_limit() {
        let opts = CloneOpts {
            filter: CloneFilter::BlobLimit { bytes: 1_000_000 },
            ..CloneOpts::default()
        };
        assert_eq!(
            render_clone_cli_flags(&opts),
            vec![
                "clone".to_string(),
                "--progress".to_string(),
                "--filter=blob:limit=1000000".to_string()
            ]
        );
    }

    #[test]
    fn render_flags_for_tree_zero_includes_sparse_and_no_checkout() {
        let opts = CloneOpts {
            filter: CloneFilter::TreeZero,
            ..CloneOpts::default()
        };
        // tree:0 alone without `--sparse --no-checkout` would immediately
        // dereference every tree during checkout and defeat the filter,
        // so the builder always emits both companion flags. If you find
        // yourself tempted to skip them, read the doc comment on
        // `render_clone_cli_flags` first.
        let flags = render_clone_cli_flags(&opts);
        assert!(flags.contains(&"--filter=tree:0".to_string()));
        assert!(flags.contains(&"--sparse".to_string()));
        assert!(flags.contains(&"--no-checkout".to_string()));
    }

    #[test]
    fn render_flags_combine_shallow_and_partial() {
        // `--depth=1 --filter=blob:none` is a real combination some CI
        // pipelines use (minimal history AND lazy blobs). The builder
        // must emit both flags side by side.
        let opts = CloneOpts {
            depth: Some(1),
            filter: CloneFilter::BlobNone,
            ..CloneOpts::default()
        };
        let flags = render_clone_cli_flags(&opts);
        assert!(flags.contains(&"--depth=1".to_string()));
        assert!(flags.contains(&"--filter=blob:none".to_string()));
    }

    #[test]
    fn clone_filter_is_enabled_only_for_real_filters() {
        assert!(!CloneFilter::None.is_enabled());
        assert!(CloneFilter::BlobNone.is_enabled());
        assert!(CloneFilter::BlobLimit { bytes: 10 }.is_enabled());
        assert!(CloneFilter::TreeZero.is_enabled());
    }

    #[test]
    fn clone_filter_display_labels_are_human_readable() {
        assert_eq!(CloneFilter::None.display_label(), "full");
        assert_eq!(CloneFilter::BlobNone.display_label(), "blob:none");
        assert_eq!(
            CloneFilter::BlobLimit { bytes: 1_048_576 }.display_label(),
            "blob:limit=1 MB"
        );
        assert_eq!(CloneFilter::TreeZero.display_label(), "tree:0 + sparse");
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("mergefox-clone-test-{tag}-{stamp}"))
    }

    fn run_git(cwd: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Build a minimal source repo with `a/one.txt` and `b/two.txt`
    /// committed on `main`. Returns the repo path and a file:// URL
    /// suitable for local clone. Returns `None` if any git command
    /// fails (e.g. no `git` in `PATH`) — tests that depend on this
    /// should `return` early and let `#[ignore]` take the blame.
    fn build_source_repo(tag: &str) -> Option<(PathBuf, String)> {
        let src = temp_dir(tag);
        std::fs::create_dir_all(&src).ok()?;
        if !run_git(&src, &["init", "-q", "-b", "main"]) {
            return None;
        }
        // Disable GPG signing + commit.gpgsign for the test — some dev
        // environments have signing on globally, which would break our
        // scripted commits.
        run_git(&src, &["config", "commit.gpgsign", "false"]);
        run_git(&src, &["config", "user.email", "test@example.com"]);
        run_git(&src, &["config", "user.name", "Clone Test"]);
        std::fs::create_dir_all(src.join("a")).ok()?;
        std::fs::create_dir_all(src.join("b")).ok()?;
        std::fs::write(src.join("a/one.txt"), b"alpha").ok()?;
        std::fs::write(src.join("b/two.txt"), b"beta").ok()?;
        if !run_git(&src, &["add", "."]) {
            return None;
        }
        if !run_git(&src, &["commit", "-q", "-m", "seed"]) {
            return None;
        }
        let url = format!("file://{}", src.display());
        Some((src, url))
    }

    #[test]
    #[ignore = "requires a working `git` binary and writes into the temp dir; run with --ignored"]
    fn blob_none_clone_records_partial_filter() {
        let Some((_src, url)) = build_source_repo("blob-none") else {
            return;
        };
        let dest = temp_dir("blob-none-dest");
        let flags = render_clone_cli_flags(&CloneOpts {
            filter: CloneFilter::BlobNone,
            ..CloneOpts::default()
        });
        let mut cmd = Command::new("git");
        for f in &flags {
            cmd.arg(f);
        }
        cmd.arg("--").arg(&url).arg(&dest);
        let ok = cmd.status().map(|s| s.success()).unwrap_or(false);
        assert!(ok, "blob:none clone failed");

        let out = Command::new("git")
            .args(["config", "--get", "remote.origin.partialclonefilter"])
            .current_dir(&dest)
            .output()
            .expect("run git config");
        let configured = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(configured, "blob:none");
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    #[ignore = "requires a working `git` binary; run with --ignored"]
    fn tree_zero_sparse_clone_only_checks_out_requested_dirs() {
        let Some((_src, url)) = build_source_repo("tree-zero") else {
            return;
        };
        let dest = temp_dir("tree-zero-dest");

        // Reuse the real `do_clone` code path so any future drift in
        // sparse-checkout setup shows up in this test.
        let progress = Arc::new(Mutex::new(CloneProgress::default()));
        let opts = CloneOpts {
            filter: CloneFilter::TreeZero,
            sparse_dirs: vec!["a".to_string()],
            ..CloneOpts::default()
        };
        let result = do_clone(&url, &dest, &opts, progress);
        assert!(result.is_ok(), "tree:0 sparse clone failed: {result:?}");

        assert!(
            dest.join("a").exists(),
            "expected sparse-included directory to be present"
        );
        assert!(
            !dest.join("b/two.txt").exists(),
            "expected sparse-excluded file to be absent"
        );
        let _ = std::fs::remove_dir_all(&dest);
    }
}

