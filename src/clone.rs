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
}

impl CloneHandle {
    pub fn poll(&self) -> Option<Result<CloneOutcome, String>> {
        self.rx.try_recv().ok()
    }

    pub fn snapshot(&self) -> CloneProgress {
        self.progress.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

pub fn spawn(
    url: String,
    dest: PathBuf,
    depth: Option<u32>,
    accounts: Vec<crate::providers::ProviderAccount>,
) -> CloneHandle {
    let progress = Arc::new(Mutex::new(CloneProgress::default()));
    let (tx, rx) = mpsc::channel();

    let url_thread = url.clone();
    let dest_thread = dest.clone();
    let progress_thread = progress.clone();

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
            do_clone_dispatched(&fetch_url, &dest_thread, depth, progress_thread)?;
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
        .map(|_| CloneOutcome {
            path: dest_thread.clone(),
            account_slug,
        })
        .map_err(|e| format!("{e:#}"));
        let _ = tx.send(result);
    });

    CloneHandle {
        url,
        dest,
        depth,
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
    depth: Option<u32>,
    progress: Arc<Mutex<CloneProgress>>,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create clone parent {}", parent.display()))?;
    }

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

    // Attempt gix first.
    let gix_err = match crate::gix_clone::do_clone_gix(url, dest, depth, progress.clone()) {
        Ok(()) => return Ok(()),
        Err(e) => format!("{e:#}"),
    };

    // gix failed. Log and fall back to the `git` CLI — but re-classify
    // the destination because gix may have written a partial skeleton
    // before erroring out.
    tracing::warn!(error = %gix_err, "gix clone failed; falling back to git CLI");
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

    match do_clone(url, dest, depth, progress) {
        // git CLI fallback — see `do_clone` below.
        Ok(()) => Ok(()),
        Err(cli_err) => {
            anyhow::bail!(
                "both git backends failed to clone {url}:\n  - gix: {gix_err}\n  - git CLI: {cli_err:#}"
            )
        }
    }
}

fn do_clone(
    url: &str,
    dest: &Path,
    depth: Option<u32>,
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

    let mut cmd = std::process::Command::new("git");
    cmd.arg("clone").arg("--progress");
    if let Some(n) = depth {
        cmd.arg(format!("--depth={n}"));
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
    Ok(())
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
