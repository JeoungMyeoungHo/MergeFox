//! Capture and restore `RepoSnapshot` without git2.
//!
//! `capture` reads refs via gix (fast, in-process). `restore_refs` writes
//! them via `git update-ref` / `git symbolic-ref` / `git checkout` so
//! hooks and config are honoured.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

use super::entry::RepoSnapshot;

/// Take a snapshot of the repo's ref state + dirty flag.
pub fn capture(repo_path: &Path) -> Result<RepoSnapshot> {
    let gix_repo = gix::discover(repo_path).context("open gix for snapshot")?;

    // HEAD: try to resolve to a commit id, fall back to empty string.
    let head = gix_repo
        .head_id()
        .ok()
        .map(|id| id.detach().to_string())
        .unwrap_or_default();

    // Head branch: if HEAD is a symbolic ref to refs/heads/<x>, get <x>.
    let head_branch = gix_repo.head_name().ok().flatten().and_then(|name| {
        let full = name.as_bstr().to_string();
        full.strip_prefix("refs/heads/").map(|s| s.to_string())
    });

    let mut refs = BTreeMap::new();
    if let Ok(platform) = gix_repo.references() {
        if let Ok(iter) = platform.all() {
            for r in iter.flatten() {
                let name = r.name().as_bstr().to_string();
                if !is_managed_ref(&name) {
                    continue;
                }
                if let Some(id) = r.target().try_id() {
                    refs.insert(name, id.to_string());
                }
            }
        }
    }

    let working_dirty = is_working_dirty(repo_path).unwrap_or(false);

    Ok(RepoSnapshot {
        head,
        head_branch,
        refs,
        working_dirty,
    })
}

fn is_working_dirty(repo_path: &Path) -> Result<bool> {
    let out = crate::git::cli::run(
        repo_path,
        ["status", "--porcelain=v1", "-z", "--untracked-files=no"],
    )?;
    Ok(!out.stdout.iter().all(|&b| b == 0 || b == b'\n'))
}

/// Move the repo's refs (+ HEAD) to match `target`.
pub fn restore_refs(repo_path: &Path, target: &RepoSnapshot) -> Result<()> {
    // 1. Update or create every managed ref in `target`.
    for (name, oid_str) in &target.refs {
        crate::git::cli::run(repo_path, ["update-ref", name, oid_str])
            .with_context(|| format!("update-ref {name} → {oid_str}"))?;
    }

    // 2. Delete refs present now but absent from the snapshot.
    let gix_repo = gix::discover(repo_path).context("open gix for restore")?;
    let mut current: Vec<String> = Vec::new();
    if let Ok(platform) = gix_repo.references() {
        if let Ok(iter) = platform.all() {
            for r in iter.flatten() {
                let name = r.name().as_bstr().to_string();
                if is_managed_ref(&name) {
                    current.push(name);
                }
            }
        }
    }
    for name in current {
        if !target.refs.contains_key(&name) {
            crate::git::cli::run(repo_path, ["update-ref", "-d", &name]).ok();
        }
    }

    // 3. Restore HEAD.
    if let Some(branch) = &target.head_branch {
        crate::git::cli::run(
            repo_path,
            ["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
        )
        .with_context(|| format!("restore HEAD → {branch}"))?;
    } else if !target.head.is_empty() {
        crate::git::cli::run(repo_path, ["update-ref", "HEAD", &target.head])
            .context("restore detached HEAD")?;
    }

    // 4. Force-checkout the new HEAD into the working tree.
    crate::git::cli::run(repo_path, ["checkout", "-f", "HEAD"])
        .context("checkout HEAD after ref restore")?;

    Ok(())
}

/// Checkout HEAD into the working tree (non-force; fails on conflicts).
pub fn checkout_head_safe(repo_path: &Path) -> Result<()> {
    crate::git::cli::run(repo_path, ["checkout", "HEAD"])?;
    Ok(())
}

fn is_managed_ref(name: &str) -> bool {
    name.starts_with("refs/heads/") || name.starts_with("refs/tags/")
}

pub use restore_refs as restore;
