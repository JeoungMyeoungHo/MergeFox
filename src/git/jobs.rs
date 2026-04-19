//! Long-running git operations (fetch / push / pull) on a background thread.
//!
//! All network I/O is delegated to the installed `git` binary so credential
//! helpers, SSH agents, and proxy config from `~/.gitconfig` all work
//! transparently — the same way they do in a terminal.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

/// Default per-job timeout. 300 s catches runaway jobs (network hang with
/// no SIGPIPE, auth loop) while comfortably covering Linux-kernel-scale
/// clones over a slow link. Overridable at runtime via
/// `MERGEFOX_GIT_TIMEOUT_SECS` — useful for CI / low-bandwidth users.
const GIT_JOB_TIMEOUT_DEFAULT_SECS: u64 = 300;

fn git_job_timeout() -> Duration {
    std::env::var("MERGEFOX_GIT_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(GIT_JOB_TIMEOUT_DEFAULT_SECS))
}

/// `.git/index.lock` / `.git/HEAD.lock` detection.
///
/// When another git process (VS Code, terminal, pre-commit hook) is
/// mid-commit, the lock file exists and our own push / commit / rebase
/// will fail with a confusing "another git process seems to be running"
/// error. Checking up front gives us an actionable message AND lets us
/// distinguish "user is busy" from "lock is stale" (mtime-based).
///
/// Returns `Ok(())` when clear, or a descriptive error when locked.
fn check_repo_not_locked(repo_path: &std::path::Path) -> Result<()> {
    let git_dir = if repo_path.join(".git").is_dir() {
        repo_path.join(".git")
    } else {
        // Already a bare repo or submodule — `.git` is a file, not a dir.
        repo_path.to_path_buf()
    };
    for lock_name in ["index.lock", "HEAD.lock"] {
        let lock = git_dir.join(lock_name);
        if !lock.exists() {
            continue;
        }
        let age = lock
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok());
        match age {
            // Lock is fresh — another git process is actively working. We
            // shouldn't touch it.
            Some(a) if a < Duration::from_secs(30) => {
                bail!(
                    "another git process is running ({} is {}s old). Try again in a moment.",
                    lock.display(),
                    a.as_secs()
                );
            }
            // Lock is stale — almost certainly left behind by a crashed
            // process. Still refuse automatically, but tell the user how
            // to recover so they don't have to google it.
            _ => {
                bail!(
                    "stale lock file detected: {}\n\nThis usually means a previous git process crashed.\nRun `rm '{}'` from a terminal to clear it.",
                    lock.display(),
                    lock.display()
                );
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub enum GitJobKind {
    Fetch {
        remote: String,
        /// Optional HTTPS credentials resolved from a connected provider
        /// account. When `Some`, we inject them via
        /// `-c credential.helper=!…` so git doesn't try to read a
        /// username from a TTY that doesn't exist.
        credentials: Option<HttpsCredentials>,
        ssh_key_path: Option<PathBuf>,
    },
    Push {
        remote: String,
        /// Full refspec, e.g. `refs/heads/main:refs/heads/main`.
        refspec: String,
        force: bool,
        set_upstream: bool,
        credentials: Option<HttpsCredentials>,
        ssh_key_path: Option<PathBuf>,
    },
    Pull {
        remote: String,
        branch: String,
        strategy: PullStrategy,
        credentials: Option<HttpsCredentials>,
        ssh_key_path: Option<PathBuf>,
    },
    /// Push one or more tags. Separate from `Push` because tag push
    /// has different semantics (no upstream, no force-with-lease
    /// relevance for annotated tags, can push many at once). `tags`
    /// empty + `all` = true maps to `git push <remote> --tags`.
    PushTag {
        remote: String,
        tags: Vec<String>,
        all: bool,
        credentials: Option<HttpsCredentials>,
        ssh_key_path: Option<PathBuf>,
    },
}

/// HTTPS credentials for a single git network op. We keep the password
/// in a normal `String` (rather than `secrecy::SecretString`) because
/// the value has to cross a `thread::spawn` boundary as an environment
/// variable anyway, and process env memory can be inspected regardless
/// of wrapper type. Lives for the duration of one command.
#[derive(Clone)]
pub struct HttpsCredentials {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for HttpsCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpsCredentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullStrategy {
    Merge,
    Rebase,
    FastForwardOnly,
}

#[derive(Debug, Default, Clone)]
pub struct JobProgress {
    pub stage: String,
    pub current: usize,
    pub total: usize,
    pub bytes: usize,
}

pub struct GitJob {
    pub kind: GitJobKind,
    pub started_at: Instant,
    pub progress: Arc<Mutex<JobProgress>>,
    cancel_requested: Arc<AtomicBool>,
    rx: Receiver<Result<(), String>>,
}

impl GitJob {
    /// Spawn the job on a background thread.
    pub fn spawn(repo_path: PathBuf, kind: GitJobKind) -> Self {
        let progress = Arc::new(Mutex::new(JobProgress {
            stage: "starting".into(),
            ..JobProgress::default()
        }));
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();

        let kind_t = kind.clone();
        let progress_t = progress.clone();
        let cancel_t = cancel_requested.clone();
        thread::spawn(move || {
            let result =
                run_job(&repo_path, kind_t, progress_t, cancel_t).map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });

        Self {
            kind,
            started_at: Instant::now(),
            progress,
            cancel_requested,
            rx,
        }
    }

    pub fn poll(&self) -> Option<Result<(), String>> {
        self.rx.try_recv().ok()
    }

    pub fn snapshot(&self) -> JobProgress {
        self.progress.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn cancel(&self) {
        self.cancel_requested.store(true, Ordering::Relaxed);
        if let Ok(mut progress) = self.progress.lock() {
            progress.stage = "cancelling".into();
        }
    }

    pub fn label(&self) -> String {
        match &self.kind {
            GitJobKind::Fetch { remote, .. } => format!("Fetching {remote}"),
            GitJobKind::Push { remote, force, .. } => {
                if *force {
                    format!("Force-pushing to {remote}")
                } else {
                    format!("Pushing to {remote}")
                }
            }
            GitJobKind::Pull {
                remote,
                branch,
                strategy,
                ..
            } => {
                let s = match strategy {
                    PullStrategy::Merge => "merge",
                    PullStrategy::Rebase => "rebase",
                    PullStrategy::FastForwardOnly => "ff-only",
                };
                format!("Pulling {remote}/{branch} ({s})")
            }
            GitJobKind::PushTag {
                remote, tags, all, ..
            } => {
                if *all {
                    format!("Pushing all tags to {remote}")
                } else if tags.len() == 1 {
                    format!("Pushing tag {} to {remote}", tags[0])
                } else {
                    format!("Pushing {} tags to {remote}", tags.len())
                }
            }
        }
    }
}

fn run_job(
    path: &std::path::Path,
    kind: GitJobKind,
    progress: Arc<Mutex<JobProgress>>,
    cancel_requested: Arc<AtomicBool>,
) -> Result<()> {
    match kind {
        GitJobKind::Fetch {
            remote,
            credentials,
            ssh_key_path,
        } => do_fetch(
            path,
            &remote,
            credentials.as_ref(),
            ssh_key_path.as_deref(),
            progress,
            cancel_requested.as_ref(),
        ),
        GitJobKind::Push {
            remote,
            refspec,
            force,
            set_upstream,
            credentials,
            ssh_key_path,
        } => do_push(
            path,
            &remote,
            &refspec,
            force,
            set_upstream,
            credentials.as_ref(),
            ssh_key_path.as_deref(),
            progress,
            cancel_requested.as_ref(),
        ),
        GitJobKind::Pull {
            remote,
            branch,
            strategy,
            credentials,
            ssh_key_path,
        } => do_pull(
            path,
            &remote,
            &branch,
            strategy,
            credentials.as_ref(),
            ssh_key_path.as_deref(),
            progress,
            cancel_requested.as_ref(),
        ),
        GitJobKind::PushTag {
            remote,
            tags,
            all,
            credentials,
            ssh_key_path,
        } => do_push_tag(
            path,
            &remote,
            &tags,
            all,
            credentials.as_ref(),
            ssh_key_path.as_deref(),
            progress,
            cancel_requested.as_ref(),
        ),
    }
}

/// Inline credential helper script used by `git -c credential.helper=…`.
///
/// The script is a POSIX function that echoes `username=…` + `password=…`
/// taken from env vars we set on the child process. Keeping the secret
/// in an env var (instead of inlining it in the command line) means:
///   * it doesn't appear in `ps` output
///   * no shell escaping for weird characters in tokens
///   * different processes can't accidentally inherit it
const INLINE_CRED_HELPER: &str =
    "!f() { printf 'username=%s\\npassword=%s\\n' \"$MERGEFOX_HTTP_USER\" \"$MERGEFOX_HTTP_PASS\"; }; f";

/// Build a `GitCommand` wrapped with the credential-helper injection if
/// we have HTTPS credentials to use. The caller then adds the actual
/// subcommand (push / pull / fetch) + its args.
fn build_cmd_with_creds(
    path: &std::path::Path,
    creds: Option<&HttpsCredentials>,
    ssh_key_path: Option<&std::path::Path>,
) -> super::cli::GitCommand {
    let mut cmd = super::cli::GitCommand::new(path);
    if let Some(c) = creds {
        // 1. CLEAR the credential helper chain. macOS ships with
        //    `credential.helper=osxkeychain` baked into the system
        //    gitconfig (Xcode CLT). Without clearing, osxkeychain runs
        //    first and may return a DIFFERENT account's token (e.g.
        //    one stored by another GUI client or `gh auth`), causing pushes to
        //    authenticate as the wrong user.
        //
        // 2. ADD our inline helper as the sole credential source.
        //
        //    Order matters: `-c credential.helper=` (empty) clears,
        //    then `-c credential.helper=!…` adds ours.
        cmd = cmd
            .arg("-c")
            .arg("credential.helper=")
            .arg("-c")
            .arg(format!("credential.helper={INLINE_CRED_HELPER}"))
            .env("MERGEFOX_HTTP_USER", &c.username)
            .env("MERGEFOX_HTTP_PASS", &c.password);
    }
    if let Some(path) = ssh_key_path {
        cmd = cmd
            .arg("-c")
            .arg(format!("core.sshCommand={}", ssh_command_for_key(path)));
    }
    cmd
}

fn ssh_command_for_key(path: &std::path::Path) -> String {
    [
        "ssh".to_string(),
        "-i".to_string(),
        path.to_string_lossy().into_owned(),
        "-o".to_string(),
        "IdentitiesOnly=yes".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
    ]
    .into_iter()
    .map(|arg| posix_shell_quote(&arg))
    .collect::<Vec<_>>()
    .join(" ")
}

fn posix_shell_quote(arg: &str) -> String {
    if arg.is_empty() {
        "''".to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\"'\"'"))
    }
}

fn do_fetch(
    path: &std::path::Path,
    remote_name: &str,
    credentials: Option<&HttpsCredentials>,
    ssh_key_path: Option<&std::path::Path>,
    progress: Arc<Mutex<JobProgress>>,
    cancel_requested: &AtomicBool,
) -> Result<()> {
    mark(&progress, "fetching");
    build_cmd_with_creds(path, credentials, ssh_key_path)
        .args(["fetch", "--prune", remote_name])
        .run_with_control(Some(cancel_requested), Some(git_job_timeout()))?;
    mark(&progress, "done");
    Ok(())
}

fn do_push(
    path: &std::path::Path,
    remote_name: &str,
    refspec: &str,
    force: bool,
    set_upstream: bool,
    credentials: Option<&HttpsCredentials>,
    ssh_key_path: Option<&std::path::Path>,
    progress: Arc<Mutex<JobProgress>>,
    cancel_requested: &AtomicBool,
) -> Result<()> {
    // Push itself doesn't touch the index, but set-upstream follows up
    // with a fetch and users reasonably expect "push failing cleanly"
    // when another git process is mid-commit in the same repo.
    check_repo_not_locked(path)?;
    mark(&progress, "pushing");
    let final_refspec = if force && !refspec.starts_with('+') {
        format!("+{refspec}")
    } else {
        refspec.to_owned()
    };
    let mut cmd = build_cmd_with_creds(path, credentials, ssh_key_path);
    cmd = cmd.arg("push");
    if set_upstream {
        cmd = cmd.arg("-u");
    }
    cmd.args([remote_name, &final_refspec])
        .run_with_control(Some(cancel_requested), Some(git_job_timeout()))?;
    if set_upstream {
        mark(&progress, "refreshing");
        let branch = refspec
            .rsplit(':')
            .next()
            .and_then(|target| target.strip_prefix("refs/heads/"))
            .unwrap_or_default();
        if !branch.is_empty() {
            build_cmd_with_creds(path, credentials, ssh_key_path)
                .args(["fetch", remote_name, branch])
                .run_with_control(Some(cancel_requested), Some(git_job_timeout()))?;
        }
    }
    mark(&progress, "done");
    Ok(())
}

/// Push one or more tags. We build a single `git push <remote> ...`
/// call with either `--tags` or an explicit list of `refs/tags/<name>`
/// refspecs. Tag push is network-only (no index touch), so we skip
/// the `.git/index.lock` pre-flight that `do_push` runs.
fn do_push_tag(
    path: &std::path::Path,
    remote_name: &str,
    tags: &[String],
    all: bool,
    credentials: Option<&HttpsCredentials>,
    ssh_key_path: Option<&std::path::Path>,
    progress: Arc<Mutex<JobProgress>>,
    cancel_requested: &AtomicBool,
) -> Result<()> {
    mark(&progress, "pushing tags");
    let mut cmd = build_cmd_with_creds(path, credentials, ssh_key_path);
    cmd = cmd.arg("push").arg(remote_name);
    if all {
        cmd = cmd.arg("--tags");
    } else {
        if tags.is_empty() {
            anyhow::bail!("no tags given and `--all` is false");
        }
        for t in tags {
            // Explicit refs/tags/<name> refspec so git doesn't treat a
            // tag name that happens to match a branch as ambiguous.
            cmd = cmd.arg(format!("refs/tags/{t}"));
        }
    }
    cmd.run_with_control(Some(cancel_requested), Some(git_job_timeout()))?;
    mark(&progress, "done");
    Ok(())
}

fn do_pull(
    path: &std::path::Path,
    remote_name: &str,
    branch: &str,
    strategy: PullStrategy,
    credentials: Option<&HttpsCredentials>,
    ssh_key_path: Option<&std::path::Path>,
    progress: Arc<Mutex<JobProgress>>,
    cancel_requested: &AtomicBool,
) -> Result<()> {
    // Merge / rebase mutate the index and working tree — fail fast with
    // a clear message if another git process is already using them.
    check_repo_not_locked(path)?;
    mark(&progress, "fetching");
    // First fetch so we have the latest remote state.
    build_cmd_with_creds(path, credentials, ssh_key_path)
        .args(["fetch", remote_name])
        .run_with_control(Some(cancel_requested), Some(git_job_timeout()))?;

    mark(&progress, "applying");
    let remote_ref = format!("{remote_name}/{branch}");

    // Merge / rebase after fetch are local ops — no credentials needed,
    // so we go through the plain `cli::run` helper.
    let args: Vec<&str> = match strategy {
        PullStrategy::FastForwardOnly => vec!["merge", "--ff-only", &remote_ref],
        PullStrategy::Merge => vec!["merge", "--no-ff", &remote_ref],
        PullStrategy::Rebase => vec!["rebase", &remote_ref],
    };
    super::cli::GitCommand::new(path)
        .args(args)
        .run_with_control(Some(cancel_requested), Some(git_job_timeout()))?;
    mark(&progress, "done");
    Ok(())
}

fn mark(p: &Arc<Mutex<JobProgress>>, stage: &str) {
    if let Ok(mut g) = p.lock() {
        g.stage = stage.to_string();
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{posix_shell_quote, ssh_command_for_key};

    #[test]
    fn posix_shell_quote_escapes_single_quotes() {
        assert_eq!(posix_shell_quote("a'b"), "'a'\"'\"'b'");
    }

    #[test]
    fn ssh_command_for_key_quotes_key_paths() {
        let cmd = ssh_command_for_key(Path::new("/tmp/key with space"));
        assert!(cmd.contains("'/tmp/key with space'"));
        assert!(cmd.contains("'IdentitiesOnly=yes'"));
        assert!(cmd.contains("'BatchMode=yes'"));
    }
}
