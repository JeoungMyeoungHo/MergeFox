//! Detect tracked binaries that should arguably live in Git LFS.
//!
//! Implementation: uses `git ls-tree -r -l HEAD` to enumerate every blob
//! in HEAD's tree with its size in one command (no per-blob object reads).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Default minimum size for a file to be flagged as an LFS candidate.
pub const DEFAULT_MIN_SIZE: u64 = 10 * 1024 * 1024;

/// Hard cap on candidate count returned.
pub const MAX_CANDIDATES: usize = 64;

/// Cap walk time so huge repos can't pin a background thread.
pub const MAX_SCAN_DURATION: Duration = Duration::from_secs(8);

#[derive(Debug, Clone)]
pub struct LfsCandidate {
    pub path: PathBuf,
    pub size: u64,
    pub oid: gix::ObjectId,
}

#[derive(Debug, Clone)]
pub struct LfsScanResult {
    pub head_oid: Option<gix::ObjectId>,
    pub candidates: Vec<LfsCandidate>,
    pub truncated: bool,
    pub total_bytes_scanned: u64,
}

/// Walk HEAD's tree and return blobs above `min_size` that aren't already
/// LFS pointers.
pub fn scan(repo_path: &Path, min_size: u64) -> Result<LfsScanResult> {
    // Resolve HEAD via git CLI.
    let head_oid = super::cli::run_line(repo_path, ["rev-parse", "HEAD"])
        .ok()
        .and_then(|s| gix::ObjectId::from_hex(s.trim().as_bytes()).ok());
    if head_oid.is_none() {
        return Ok(LfsScanResult {
            head_oid: None,
            candidates: Vec::new(),
            truncated: false,
            total_bytes_scanned: 0,
        });
    }

    // `-l` adds the size column; `-r` recurses into trees.
    // Output per line: `<mode> <type> <sha> <size>\t<path>`
    let out =
        super::cli::run(repo_path, ["ls-tree", "-r", "-l", "HEAD"]).context("git ls-tree HEAD")?;

    // Open gix for LFS-pointer content inspection.
    let gix_repo = gix::discover(repo_path).ok();

    let started = Instant::now();
    let mut candidates: Vec<LfsCandidate> = Vec::new();
    let mut truncated = false;
    let mut total_bytes: u64 = 0;

    for line in out.stdout_str().lines() {
        if started.elapsed() > MAX_SCAN_DURATION {
            truncated = true;
            break;
        }
        if candidates.len() >= MAX_CANDIDATES {
            truncated = true;
            break;
        }

        // Split into metadata and path on the tab.
        let (meta, path_part) = match line.split_once('\t') {
            Some(p) => p,
            None => continue,
        };
        let mut tokens = meta.split_whitespace();
        let _mode = tokens.next();
        let kind = tokens.next();
        let sha = tokens.next();
        let size_str = tokens.next();
        if kind != Some("blob") {
            continue;
        }
        let Some(sha) = sha else { continue };
        let Some(size_str) = size_str else { continue };
        let Ok(oid) = gix::ObjectId::from_hex(sha.as_bytes()) else {
            continue;
        };
        // `git ls-tree -l` shows `-` as size for non-blobs; blobs always have a number.
        let size: u64 = match size_str.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        total_bytes = total_bytes.saturating_add(size);

        if size < min_size {
            continue;
        }

        // Check if this blob is already an LFS pointer (short text file).
        if let Some(ref gix_repo) = gix_repo {
            if is_blob_lfs_pointer(gix_repo, oid) {
                continue;
            }
        }

        candidates.push(LfsCandidate {
            path: PathBuf::from(path_part),
            size,
            oid,
        });
    }

    candidates.sort_unstable_by(|a, b| b.size.cmp(&a.size));

    Ok(LfsScanResult {
        head_oid,
        candidates,
        truncated,
        total_bytes_scanned: total_bytes,
    })
}

fn is_blob_lfs_pointer(gix_repo: &gix::Repository, oid: gix::ObjectId) -> bool {
    let obj = match gix_repo.find_object(oid) {
        Ok(o) => o,
        Err(_) => return false,
    };
    let blob = match obj.try_into_blob() {
        Ok(b) => b,
        Err(_) => return false,
    };
    let content = blob.data.as_slice();
    const POINTER_PREFIX: &[u8] = b"version https://git-lfs.github.com/spec/";
    if content.len() > 1024 {
        return false;
    }
    content.starts_with(POINTER_PREFIX)
}
