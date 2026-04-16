use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use super::graph::{CommitGraph, GraphScope};
use crate::actions::ResetMode;

pub struct Repo {
    gix: gix::Repository,
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct BranchInfo {
    pub name: String,
    pub is_head: bool,
    pub is_remote: bool,
    pub upstream: Option<String>,
    pub last_commit_summary: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ConflictBlob {
    pub oid: Option<gix::ObjectId>,
    pub size: usize,
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ConflictEntry {
    pub path: PathBuf,
    pub ancestor: Option<ConflictBlob>,
    pub ours: Option<ConflictBlob>,
    pub theirs: Option<ConflictBlob>,
    pub merged_text: Option<String>,
    pub is_binary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictChoice {
    Ours,
    Theirs,
}

#[derive(Debug, Clone)]
pub struct LinearCommit {
    pub oid: gix::ObjectId,
    pub summary: String,
    pub message: String,
    pub author: String,
    pub timestamp: i64,
    pub parent: Option<gix::ObjectId>,
}

#[derive(Debug, Clone)]
pub struct ReflogEntrySummary {
    pub index: usize,
    pub old_oid: gix::ObjectId,
    pub new_oid: gix::ObjectId,
    pub message: String,
    pub committer: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone)]
pub struct RemoteInfo {
    pub name: String,
    pub fetch_url: Option<String>,
    pub push_url: Option<String>,
}

/// Pending repo operation, detected via marker files inside `.git/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoState {
    Clean,
    Merge,
    Revert,
    RevertSequence,
    CherryPick,
    CherryPickSequence,
    Bisect,
    Rebase,
    RebaseInteractive,
    RebaseMerge,
    ApplyMailbox,
    ApplyMailboxOrRebase,
}

#[derive(Debug, Clone, Copy)]
pub struct AutoStashOpts {
    pub size_limit_bytes: u64,
}

impl Default for AutoStashOpts {
    fn default() -> Self {
        Self {
            size_limit_bytes: 100 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub enum AutoStashOutcome {
    Clean,
    Stashed { oid: gix::ObjectId, bytes: u64 },
    Refused { reason: AutoStashRefusal },
}

#[derive(Debug, thiserror::Error)]
pub enum AutoStashRefusal {
    #[error(
        "auto-stash skipped: {} MB of dirty tracked files exceed {} MB limit; \
         commit, discard, or raise the limit before retrying",
        bytes / 1_048_576,
        limit / 1_048_576
    )]
    TooLarge { bytes: u64, limit: u64 },
}

/// How long mergeFox auto-stashes are kept before retention pruning.
pub const AUTOSTASH_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;

impl Repo {
    pub fn open(path: &Path) -> Result<Self> {
        let gix = gix::discover(path)
            .with_context(|| format!("no git repository at {}", path.display()))?;
        let path = gix
            .work_dir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.to_path_buf());
        let mut repo = Self { gix, path };

        if let Err(e) = repo.prune_autostashes(AUTOSTASH_RETENTION_SECS) {
            eprintln!("mergefox: auto-stash retention prune failed: {e:#}");
        }
        Ok(repo)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Expose the gix handle for read-path callers (blob loading, etc.).
    pub fn gix(&self) -> &gix::Repository {
        &self.gix
    }

    pub fn head_name(&self) -> Option<String> {
        self.gix.head_name().ok().flatten().map(|name| {
            let bytes = name.shorten();
            String::from_utf8_lossy(bytes).into_owned()
        })
    }

    pub fn head_oid(&self) -> Option<gix::ObjectId> {
        self.gix.head_id().ok().map(|id| id.detach())
    }

    pub fn state(&self) -> RepoState {
        let git_dir = self.gix.git_dir();
        if git_dir.join("rebase-merge").is_dir() {
            if git_dir.join("rebase-merge").join("interactive").exists() {
                return RepoState::RebaseInteractive;
            }
            return RepoState::RebaseMerge;
        }
        if git_dir.join("rebase-apply").is_dir() {
            if git_dir.join("rebase-apply").join("applying").exists() {
                return RepoState::ApplyMailbox;
            }
            return RepoState::ApplyMailboxOrRebase;
        }
        if git_dir.join("CHERRY_PICK_HEAD").exists() {
            if git_dir.join("sequencer").is_dir() {
                return RepoState::CherryPickSequence;
            }
            return RepoState::CherryPick;
        }
        if git_dir.join("REVERT_HEAD").exists() {
            if git_dir.join("sequencer").is_dir() {
                return RepoState::RevertSequence;
            }
            return RepoState::Revert;
        }
        if git_dir.join("MERGE_HEAD").exists() {
            return RepoState::Merge;
        }
        if git_dir.join("BISECT_LOG").exists() {
            return RepoState::Bisect;
        }
        RepoState::Clean
    }

    pub fn list_branches(&self, include_remote: bool) -> Result<Vec<BranchInfo>> {
        let head_full_name = self.gix.head_name().ok().flatten();
        let head_short = head_full_name
            .as_ref()
            .map(|n| String::from_utf8_lossy(n.shorten()).into_owned());

        let platform = self.gix.references().context("open ref iterator")?;
        let mut out = Vec::new();

        for r in platform.prefixed("refs/heads/")?.flatten() {
            let full = r.name();
            let short = String::from_utf8_lossy(full.shorten()).into_owned();
            let last_commit_summary = peel_summary(&self.gix, &r);
            let upstream = local_branch_upstream(&self.gix, &short);
            let is_head = head_short.as_deref() == Some(short.as_str());
            out.push(BranchInfo {
                name: short,
                is_head,
                is_remote: false,
                upstream,
                last_commit_summary,
            });
        }

        if include_remote {
            for r in platform.prefixed("refs/remotes/")?.flatten() {
                let full = r.name();
                let short = String::from_utf8_lossy(full.shorten()).into_owned();
                if short.ends_with("/HEAD") {
                    continue;
                }
                let last_commit_summary = peel_summary(&self.gix, &r);
                out.push(BranchInfo {
                    name: short,
                    is_head: false,
                    is_remote: true,
                    upstream: None,
                    last_commit_summary,
                });
            }
        }

        out.sort_by(|a, b| {
            a.is_remote
                .cmp(&b.is_remote)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(out)
    }

    pub fn build_graph(&self, scope: GraphScope) -> Result<CommitGraph> {
        CommitGraph::build(&self.gix, scope)
    }

    pub fn linear_head_commits(&self, limit: usize) -> Result<Vec<LinearCommit>> {
        let mut out = Vec::new();
        let mut current_id = self.gix.head_id().context("no HEAD")?.detach();
        loop {
            let commit = self
                .gix
                .find_object(current_id)
                .context("read commit")?
                .try_into_commit()
                .context("HEAD is not a commit")?;
            let parent_ids: Vec<gix::ObjectId> =
                commit.parent_ids().map(|id| id.detach()).collect();
            if parent_ids.len() > 1 {
                bail!("interactive rebase currently supports only linear history");
            }
            let parent_gix = parent_ids.first().copied();
            let (summary, body, author_name, timestamp) = decode_commit_meta(&commit)?;
            let full_msg = if body.is_empty() {
                summary.clone()
            } else {
                format!("{summary}\n\n{body}")
            };
            out.push(LinearCommit {
                oid: current_id,
                summary,
                message: full_msg,
                author: author_name,
                timestamp,
                parent: parent_gix,
            });

            if out.len() >= limit {
                break;
            }
            let Some(next) = parent_gix else { break };
            current_id = next;
        }

        out.reverse();
        Ok(out)
    }

    pub fn head_reflog(&self, limit: usize) -> Result<Vec<ReflogEntrySummary>> {
        let n = limit.to_string();
        let out = super::cli::run(
            &self.path,
            [
                "reflog",
                "--format=%H\x1f%P\x1f%cn\x1f%ct\x1f%gs",
                "-n",
                &n,
                "HEAD",
            ],
        )?;
        let stdout = out.stdout_str();
        let mut entries = Vec::new();
        for (idx, line) in stdout.lines().enumerate() {
            let parts: Vec<&str> = line.splitn(5, '\x1f').collect();
            if parts.len() < 5 {
                continue;
            }
            let new_oid = match gix::ObjectId::from_hex(parts[0].trim().as_bytes()) {
                Ok(o) => o,
                Err(_) => continue,
            };
            let old_oid_str = parts[1].split_whitespace().next().unwrap_or("");
            let old_oid = gix::ObjectId::from_hex(old_oid_str.as_bytes())
                .unwrap_or_else(|_| gix::ObjectId::null(gix::hash::Kind::Sha1));
            let committer = parts[2].to_owned();
            let timestamp = parts[3].parse::<i64>().unwrap_or(0);
            let message = parts[4].to_owned();
            entries.push(ReflogEntrySummary {
                index: idx,
                old_oid,
                new_oid,
                message,
                committer,
                timestamp,
            });
        }
        Ok(entries)
    }

    pub fn conflict_entries(&self) -> Result<Vec<ConflictEntry>> {
        // List conflicted files via `git diff --name-only --diff-filter=U`
        // then load the three blob stages via `git show :N:<path>`.
        let status_out =
            super::cli::run(&self.path, ["diff", "--name-only", "--diff-filter=U", "-z"])?;
        let paths: Vec<PathBuf> = status_out
            .stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(String::from_utf8_lossy(s).as_ref()))
            .collect();

        if paths.is_empty() {
            // Also check unmerged entries from `git status -z --porcelain=v1`.
            // The diff filter above only catches U entries after the index
            // has conflicts; on a fresh cherry-pick the index may show
            // AA / DD instead.
            let status = super::cli::run(&self.path, ["status", "--porcelain=v1", "-z"])?;
            if !status.stdout.is_empty() {
                // Parse conflicted paths from status
                let entries = super::ops::status_entries(&self.path)?;
                if entries.iter().all(|e| !e.conflicted) {
                    return Ok(Vec::new());
                }
            } else {
                return Ok(Vec::new());
            }
        }

        let mut out = Vec::new();
        for path in paths {
            let ancestor = load_conflict_stage(&self.path, &path, 1);
            let ours = load_conflict_stage(&self.path, &path, 2);
            let theirs = load_conflict_stage(&self.path, &path, 3);
            let merged_text = worktree_text(&self.path.join(&path));
            let is_binary = [&ancestor, &ours, &theirs]
                .iter()
                .filter_map(|s| s.as_ref())
                .any(|b| b.text.is_none() && b.size > 0);
            out.push(ConflictEntry {
                path,
                ancestor,
                ours,
                theirs,
                merged_text,
                is_binary,
            });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    pub fn resolve_conflict_choice(&self, path: &Path, choice: ConflictChoice) -> Result<()> {
        let stage = match choice {
            ConflictChoice::Ours => 2,
            ConflictChoice::Theirs => 3,
        };
        // Read the blob at stage N and write it to the working tree.
        let blob = super::cli::run(
            &self.path,
            ["show", &format!(":{stage}:{}", path.display())],
        );
        let abs = self.path.join(path);
        match blob {
            Ok(out) => {
                if let Some(parent) = abs.parent() {
                    fs::create_dir_all(parent).ok();
                }
                fs::write(&abs, &out.stdout).with_context(|| format!("write {}", abs.display()))?;
            }
            Err(_) => {
                // Stage doesn't exist → the file was deleted on that side.
                let _ = fs::remove_file(&abs);
            }
        }
        // Stage the result.
        super::cli::run(&self.path, ["add", "--", &path.display().to_string()])?;
        Ok(())
    }

    pub fn resolve_conflict_manual(&self, path: &Path, merged_text: &str) -> Result<()> {
        let abs = self.path.join(path);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(&abs, merged_text).with_context(|| format!("write {}", abs.display()))?;
        super::cli::run(&self.path, ["add", "--", &path.display().to_string()])?;
        Ok(())
    }

    pub fn pending_operation_has_conflicts(&self) -> Result<bool> {
        let entries = self.conflict_entries()?;
        Ok(!entries.is_empty())
    }

    pub fn start_cherry_pick_apply(&self, oid: gix::ObjectId) -> Result<bool> {
        let result = super::cli::run(&self.path, ["cherry-pick", "--no-commit", &oid.to_string()]);
        match result {
            Ok(_) => Ok(false),
            Err(_) => {
                // cherry-pick may fail due to conflicts — check if conflicts exist.
                let has_conflicts = self.pending_operation_has_conflicts().unwrap_or(false);
                if has_conflicts {
                    Ok(true)
                } else {
                    // Real error
                    Err(anyhow::anyhow!("cherry-pick failed"))
                }
            }
        }
    }

    pub fn finish_pending_pick_commit(
        &self,
        _source_oid: gix::ObjectId,
        message_override: Option<&str>,
    ) -> Result<gix::ObjectId> {
        // The cherry-pick already set up CHERRY_PICK_HEAD with the right
        // author; `git cherry-pick --continue` creates the commit.
        // If a message override is provided, set it via COMMIT_EDITMSG.
        let git_dir = self.git_dir();
        if let Some(msg) = message_override {
            fs::write(git_dir.join("COMMIT_EDITMSG"), msg)?;
        }
        super::cli::run(&self.path, ["cherry-pick", "--continue"])
            .context("cherry-pick --continue")?;
        head_oid_cli(&self.path)
    }

    pub fn finish_pending_pick_squash(&self, message: &str) -> Result<gix::ObjectId> {
        super::cli::GitCommand::new(&self.path)
            .args(["commit", "--amend", "-F", "-"])
            .stdin(message.as_bytes().to_vec())
            .run()
            .context("squash commit")?;
        head_oid_cli(&self.path)
    }

    pub fn continue_merge(&self) -> Result<gix::ObjectId> {
        super::cli::run(&self.path, ["merge", "--continue"]).context("merge --continue")?;
        head_oid_cli(&self.path)
    }

    pub fn continue_cherry_pick(&self) -> Result<gix::ObjectId> {
        super::cli::run(&self.path, ["cherry-pick", "--continue"])
            .context("cherry-pick --continue")?;
        head_oid_cli(&self.path)
    }

    pub fn continue_revert(&self) -> Result<gix::ObjectId> {
        super::cli::run(&self.path, ["revert", "--continue"]).context("revert --continue")?;
        head_oid_cli(&self.path)
    }

    pub fn abort_operation(&self) -> Result<()> {
        let args: &[&str] = match self.state() {
            RepoState::Merge => &["merge", "--abort"],
            RepoState::CherryPick | RepoState::CherryPickSequence => &["cherry-pick", "--abort"],
            RepoState::Revert | RepoState::RevertSequence => &["revert", "--abort"],
            RepoState::Rebase
            | RepoState::RebaseInteractive
            | RepoState::RebaseMerge
            | RepoState::ApplyMailboxOrRebase => &["rebase", "--abort"],
            _ => bail!("no abortable repository operation in progress"),
        };
        super::cli::run(&self.path, args)?;
        Ok(())
    }

    pub fn create_backup_branch(&self, branch: &str) -> Result<String> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for suffix in 0..100u32 {
            let name = if suffix == 0 {
                format!("{branch}.backup-{ts}")
            } else {
                format!("{branch}.backup-{ts}-{suffix}")
            };
            // Try to create; fail means it exists → try next suffix.
            if super::cli::run(&self.path, ["branch", &name]).is_ok() {
                return Ok(name);
            }
        }
        bail!("unable to allocate backup branch name for {branch}")
    }

    pub fn create_recovery_branch(&self, at: gix::ObjectId) -> Result<String> {
        let short = short_sha(&at);
        for suffix in 0..100u32 {
            let name = if suffix == 0 {
                format!("recovery-{short}")
            } else {
                format!("recovery-{short}-{suffix}")
            };
            if super::cli::run(&self.path, ["branch", &name, &at.to_string()]).is_ok() {
                return Ok(name);
            }
        }
        bail!("unable to allocate recovery branch for {at}")
    }

    pub fn list_remotes(&self) -> Result<Vec<RemoteInfo>> {
        let mut out = Vec::new();
        for name in self.gix.remote_names() {
            let name_str = name.to_string();
            let remote = self
                .gix
                .find_remote(name.as_ref())
                .with_context(|| format!("find remote {name_str}"))?;
            let fetch_url = remote
                .url(gix::remote::Direction::Fetch)
                .map(|u| u.to_bstring().to_string());
            let push_url = remote
                .url(gix::remote::Direction::Push)
                .map(|u| u.to_bstring().to_string());
            let push_url = if push_url == fetch_url {
                None
            } else {
                push_url
            };
            out.push(RemoteInfo {
                name: name_str,
                fetch_url,
                push_url,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn add_remote(&self, name: &str, fetch_url: &str, push_url: Option<&str>) -> Result<()> {
        super::cli::run(&self.path, ["remote", "add", name, fetch_url])?;
        if let Some(url) = push_url {
            super::cli::run(&self.path, ["remote", "set-url", "--push", name, url])?;
        }
        Ok(())
    }

    pub fn update_remote_urls(
        &self,
        name: &str,
        fetch_url: &str,
        push_url: Option<&str>,
    ) -> Result<()> {
        super::cli::run(&self.path, ["remote", "set-url", name, fetch_url])?;
        match push_url {
            Some(url) => {
                super::cli::run(&self.path, ["remote", "set-url", "--push", name, url])?;
            }
            None => {
                // Remove push override so it falls back to the fetch URL.
                let _ = super::cli::run(
                    &self.path,
                    ["remote", "set-url", "--push", "--delete", name, ".*"],
                );
            }
        }
        Ok(())
    }

    pub fn delete_remote(&self, name: &str) -> Result<()> {
        super::cli::run(&self.path, ["remote", "remove", name])?;
        Ok(())
    }

    pub fn staged_diff_text(&self, max_bytes: usize) -> Result<String> {
        let staged = super::cli::run(&self.path, ["diff", "--cached", "--no-color", "-U3"])?;
        let mut text = staged.stdout_str();
        if text.trim().is_empty() {
            let unstaged = super::cli::run(&self.path, ["diff", "HEAD", "--no-color", "-U3"])?;
            text = unstaged.stdout_str();
        }
        if text.len() > max_bytes {
            text.truncate(max_bytes);
            text.push_str("\n… (diff truncated by mergeFox before prompting)\n");
        }
        Ok(text)
    }

    pub fn auto_stash(&mut self, reason: &str, opts: AutoStashOpts) -> Result<AutoStashOutcome> {
        auto_stash_path(&self.path, reason, opts)
    }

    pub fn auto_stash_if_dirty(&mut self, reason: &str) -> Result<bool> {
        match self.auto_stash(reason, AutoStashOpts::default())? {
            AutoStashOutcome::Clean => Ok(false),
            AutoStashOutcome::Stashed { .. } => Ok(true),
            AutoStashOutcome::Refused { reason } => bail!(reason.to_string()),
        }
    }

    pub fn prune_autostashes(&mut self, max_age_secs: u64) -> Result<usize> {
        let stashes = super::ops::stash_list(&self.path).unwrap_or_default();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let mut victims: Vec<usize> = Vec::new();
        for stash in &stashes {
            if !stash.message.contains("mergefox: auto-stash ") {
                continue;
            }
            // Get commit timestamp via git CLI — avoids borrow-lifetime
            // pain with gix's temporary author/sig objects.
            let ts: i64 = super::cli::run_line(
                &self.path,
                ["show", "-s", "--format=%at", &stash.oid.to_string()],
            )
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
            let age = now.saturating_sub(ts.max(0) as u64);
            if age > max_age_secs {
                victims.push(stash.index);
            }
        }

        // Drop highest-index first so earlier indices stay stable.
        victims.sort_unstable_by(|a, b| b.cmp(a));
        let mut dropped = 0usize;
        for idx in victims {
            let refspec = format!("stash@{{{idx}}}");
            if super::cli::run(&self.path, ["stash", "drop", &refspec]).is_ok() {
                dropped += 1;
            }
        }
        Ok(dropped)
    }

    pub fn git_dir(&self) -> &Path {
        self.gix.git_dir()
    }

    pub fn checkout_branch(&self, name: &str) -> Result<()> {
        // Use `git checkout -f` so we override local modifications (callers
        // are expected to auto-stash first).
        super::cli::run(&self.path, ["checkout", "-f", name])
            .with_context(|| format!("checkout branch {name}"))?;
        Ok(())
    }

    pub fn checkout_commit(&self, oid: gix::ObjectId) -> Result<()> {
        super::cli::run(&self.path, ["checkout", "-f", &oid.to_string()])
            .with_context(|| format!("checkout commit {oid}"))?;
        Ok(())
    }

    pub fn revert_commit(&self, oid: gix::ObjectId) -> Result<gix::ObjectId> {
        super::cli::run(&self.path, ["revert", "--no-edit", &oid.to_string()])
            .with_context(|| format!("revert {oid}"))?;
        head_oid_cli(&self.path)
    }

    pub fn cherry_pick_commit(&self, oid: gix::ObjectId) -> Result<gix::ObjectId> {
        super::cli::run(&self.path, ["cherry-pick", &oid.to_string()])
            .with_context(|| format!("cherry-pick {oid}"))?;
        head_oid_cli(&self.path)
    }

    pub fn reset(&self, mode: ResetMode, target: gix::ObjectId) -> Result<()> {
        let flag = match mode {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
        };
        super::cli::run(&self.path, ["reset", flag, &target.to_string()])
            .with_context(|| format!("reset {flag} → {target}"))?;
        Ok(())
    }

    pub fn create_branch(&self, name: &str, at: gix::ObjectId) -> Result<()> {
        super::cli::run(&self.path, ["branch", name, &at.to_string()])
            .with_context(|| format!("create branch {name}"))?;
        Ok(())
    }

    pub fn delete_branch(&self, name: &str, is_remote: bool) -> Result<()> {
        if is_remote {
            // Remote-tracking branch: delete the local ref
            super::cli::run(&self.path, ["branch", "-r", "-d", name])
                .with_context(|| format!("delete remote branch {name}"))?;
        } else {
            super::cli::run(&self.path, ["branch", "-d", name])
                .with_context(|| format!("delete branch {name}"))?;
        }
        Ok(())
    }

    pub fn rename_branch(&self, from: &str, to: &str) -> Result<()> {
        super::cli::run(&self.path, ["branch", "-m", from, to])
            .with_context(|| format!("rename {from} → {to}"))?;
        Ok(())
    }

    pub fn set_upstream(&self, branch: &str, upstream: Option<&str>) -> Result<()> {
        match upstream {
            Some(u) => super::cli::run(&self.path, ["branch", "--set-upstream-to", u, branch])
                .with_context(|| format!("set upstream {u} on {branch}"))?,
            None => super::cli::run(&self.path, ["branch", "--unset-upstream", branch])
                .with_context(|| format!("unset upstream on {branch}"))?,
        };
        Ok(())
    }

    pub fn create_tag(
        &self,
        name: &str,
        at: gix::ObjectId,
        message: Option<&str>,
    ) -> Result<gix::ObjectId> {
        match message {
            Some(msg) if !msg.is_empty() => {
                super::cli::GitCommand::new(&self.path)
                    .args(["tag", "-a", name, &at.to_string(), "-m", msg])
                    .run()
                    .with_context(|| format!("create annotated tag {name}"))?;
            }
            _ => {
                super::cli::run(&self.path, ["tag", name, &at.to_string()])
                    .with_context(|| format!("create lightweight tag {name}"))?;
            }
        }
        // Return the tag object OID (for annotated) or the commit OID.
        let s = super::cli::run_line(&self.path, ["rev-parse", name])?;
        gix::ObjectId::from_hex(s.trim().as_bytes()).context("parse tag OID")
    }

    pub fn tip_of(&self, branch: &str, is_remote: bool) -> Result<gix::ObjectId> {
        let full = if is_remote {
            format!("refs/remotes/{branch}")
        } else {
            format!("refs/heads/{branch}")
        };
        let mut reference = self
            .gix
            .find_reference(full.as_str())
            .with_context(|| format!("branch {branch} not found"))?;
        let id = reference
            .peel_to_id_in_place()
            .with_context(|| format!("branch {branch} has no target"))?;
        Ok(id.detach())
    }
}

/// Auto-stash path variant used by background journal-nav threads (which
/// can't borrow `Repo` across thread boundaries but DO have the repo path).
pub fn auto_stash_path(
    repo_path: &Path,
    reason: &str,
    opts: AutoStashOpts,
) -> Result<AutoStashOutcome> {
    // 1. Cheap dirty probe — tracked files only.
    let status = super::cli::run(
        repo_path,
        ["status", "--porcelain=v1", "-z", "--untracked-files=no"],
    )?;
    let dirty = !status.stdout.iter().all(|&b| b == 0 || b == b'\n');
    if !dirty {
        return Ok(AutoStashOutcome::Clean);
    }

    // 2. Pre-flight size estimate.
    let size = estimate_dirty_tracked_size(repo_path);
    if size > opts.size_limit_bytes {
        return Ok(AutoStashOutcome::Refused {
            reason: AutoStashRefusal::TooLarge {
                bytes: size,
                limit: opts.size_limit_bytes,
            },
        });
    }

    // 3. Stash tracked-dirty files only (no untracked — fast on large repos).
    let msg = format!("mergefox: auto-stash before {reason}");
    super::cli::run(repo_path, ["stash", "push", "-m", &msg]).context("auto-stash")?;
    let s = super::cli::run_line(repo_path, ["rev-parse", "stash@{0}"]).unwrap_or_default();
    let oid = gix::ObjectId::from_hex(s.trim().as_bytes())
        .unwrap_or_else(|_| gix::ObjectId::null(gix::hash::Kind::Sha1));
    Ok(AutoStashOutcome::Stashed { oid, bytes: size })
}

/// Free-function form for the background undo/redo thread.
pub fn auto_stash_repository(
    repo_path: &Path,
    reason: &str,
    opts: AutoStashOpts,
) -> Result<AutoStashOutcome> {
    auto_stash_path(repo_path, reason, opts)
}

fn estimate_dirty_tracked_size(repo_path: &Path) -> u64 {
    let Ok(status) = super::cli::run(
        repo_path,
        ["status", "--porcelain=v1", "-z", "--untracked-files=no"],
    ) else {
        return 0;
    };
    let workdir = repo_path;
    let mut total = 0u64;
    // Parse path tokens from -z output (skip the XY prefix).
    let data = &status.stdout;
    let mut pos = 0;
    while pos < data.len() {
        if pos + 3 > data.len() {
            break;
        }
        pos += 3; // skip XY + space
        let path_start = pos;
        while pos < data.len() && data[pos] != 0 {
            pos += 1;
        }
        let path_bytes = &data[path_start..pos];
        if pos < data.len() {
            pos += 1;
        }
        let path = std::path::Path::new(std::str::from_utf8(path_bytes).unwrap_or(""));
        let abs = workdir.join(path);
        if let Ok(meta) = fs::metadata(&abs) {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

fn head_oid_cli(repo_path: &Path) -> Result<gix::ObjectId> {
    let s = super::cli::run_line(repo_path, ["rev-parse", "HEAD"])?;
    gix::ObjectId::from_hex(s.trim().as_bytes()).context("parse HEAD OID")
}

fn short_sha(oid: &gix::ObjectId) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}

fn read_oid_file(path: PathBuf) -> Result<gix::ObjectId> {
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let trimmed = text.lines().next().unwrap_or("").trim();
    gix::ObjectId::from_hex(trimmed.as_bytes())
        .with_context(|| format!("parse oid in {}", path.display()))
}

fn load_conflict_stage(repo_path: &Path, path: &Path, stage: u8) -> Option<ConflictBlob> {
    let spec = format!(":{stage}:{}", path.display());
    let out = super::cli::run(repo_path, ["show", &spec]).ok()?;
    let bytes = out.stdout;
    let size = bytes.len();
    let text = std::str::from_utf8(&bytes).ok().map(str::to_owned);
    // Get OID via `git ls-files --stage` and match the requested stage.
    //
    // Format per line: `<mode> <sha> <stage>\t<path>`
    let stage_str = stage.to_string();
    let oid = super::cli::run(
        repo_path,
        ["ls-files", "--stage", "--", &path.display().to_string()],
    )
    .ok()
    .and_then(|o| {
        o.stdout_str().lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            let _mode = parts.next()?;
            let sha = parts.next()?;
            let st = parts.next()?;
            if st == stage_str {
                gix::ObjectId::from_hex(sha.as_bytes()).ok()
            } else {
                None
            }
        })
    });
    Some(ConflictBlob { oid, size, text })
}

fn worktree_text(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    std::str::from_utf8(&bytes).ok().map(str::to_owned)
}

fn peel_summary(gix: &gix::Repository, r: &gix::Reference<'_>) -> Option<String> {
    let id = r.target().try_id()?.to_owned();
    let commit = gix.find_object(id).ok()?.try_into_commit().ok()?;
    let msg = commit.message().ok()?;
    Some(msg.summary().to_string())
}

fn local_branch_upstream(gix: &gix::Repository, short_name: &str) -> Option<String> {
    let snap = gix.config_snapshot();
    let remote = snap
        .string(format!("branch.{short_name}.remote").as_str())?
        .to_string();
    let merge = snap
        .string(format!("branch.{short_name}.merge").as_str())?
        .to_string();
    let branch = merge.strip_prefix("refs/heads/").unwrap_or(&merge);
    Some(format!("{remote}/{branch}"))
}

fn decode_commit_meta(commit: &gix::Commit<'_>) -> Result<(String, String, String, i64)> {
    let message = commit.message().context("commit message")?;
    let summary = message.summary().to_string();
    let body = message.body.map(|b| b.to_string()).unwrap_or_default();
    let author = commit.author().context("commit author")?;
    let author_name = author.name.to_string();
    let timestamp = author.time().map(|t| t.seconds).unwrap_or(0);
    Ok((summary, body, author_name, timestamp))
}
