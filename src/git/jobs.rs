//! Long-running git operations (fetch / push / pull) on a background thread.
//!
//! All network I/O is delegated to the installed `git` binary so credential
//! helpers, SSH agents, and proxy config from `~/.gitconfig` all work
//! transparently — the same way they do in a terminal.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use anyhow::Result;

#[derive(Debug, Clone)]
pub enum GitJobKind {
    Fetch {
        remote: String,
        /// Optional HTTPS credentials resolved from a connected provider
        /// account. When `Some`, we inject them via
        /// `-c credential.helper=!…` so git doesn't try to read a
        /// username from a TTY that doesn't exist.
        credentials: Option<HttpsCredentials>,
    },
    Push {
        remote: String,
        /// Full refspec, e.g. `refs/heads/main:refs/heads/main`.
        refspec: String,
        force: bool,
        set_upstream: bool,
        credentials: Option<HttpsCredentials>,
    },
    Pull {
        remote: String,
        branch: String,
        strategy: PullStrategy,
        credentials: Option<HttpsCredentials>,
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
    rx: Receiver<Result<(), String>>,
}

impl GitJob {
    /// Spawn the job on a background thread.
    pub fn spawn(repo_path: PathBuf, kind: GitJobKind) -> Self {
        let progress = Arc::new(Mutex::new(JobProgress {
            stage: "starting".into(),
            ..JobProgress::default()
        }));
        let (tx, rx) = mpsc::channel();

        let kind_t = kind.clone();
        let progress_t = progress.clone();
        thread::spawn(move || {
            let result = run_job(&repo_path, kind_t, progress_t).map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });

        Self {
            kind,
            started_at: Instant::now(),
            progress,
            rx,
        }
    }

    pub fn poll(&self) -> Option<Result<(), String>> {
        self.rx.try_recv().ok()
    }

    pub fn snapshot(&self) -> JobProgress {
        self.progress.lock().map(|g| g.clone()).unwrap_or_default()
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
        }
    }
}

fn run_job(
    path: &std::path::Path,
    kind: GitJobKind,
    progress: Arc<Mutex<JobProgress>>,
) -> Result<()> {
    match kind {
        GitJobKind::Fetch {
            remote,
            credentials,
        } => do_fetch(path, &remote, credentials.as_ref(), progress),
        GitJobKind::Push {
            remote,
            refspec,
            force,
            set_upstream,
            credentials,
        } => do_push(
            path,
            &remote,
            &refspec,
            force,
            set_upstream,
            credentials.as_ref(),
            progress,
        ),
        GitJobKind::Pull {
            remote,
            branch,
            strategy,
            credentials,
        } => do_pull(
            path,
            &remote,
            &branch,
            strategy,
            credentials.as_ref(),
            progress,
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
) -> super::cli::GitCommand {
    let mut cmd = super::cli::GitCommand::new(path);
    if let Some(c) = creds {
        // 1. CLEAR the credential helper chain. macOS ships with
        //    `credential.helper=osxkeychain` baked into the system
        //    gitconfig (Xcode CLT). Without clearing, osxkeychain runs
        //    first and may return a DIFFERENT account's token (e.g.
        //    one stored by GitKraken or `gh auth`), causing pushes to
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
    cmd
}

fn do_fetch(
    path: &std::path::Path,
    remote_name: &str,
    credentials: Option<&HttpsCredentials>,
    progress: Arc<Mutex<JobProgress>>,
) -> Result<()> {
    mark(&progress, "fetching");
    build_cmd_with_creds(path, credentials)
        .args(["fetch", "--prune", remote_name])
        .run()?;
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
    progress: Arc<Mutex<JobProgress>>,
) -> Result<()> {
    mark(&progress, "pushing");
    let final_refspec = if force && !refspec.starts_with('+') {
        format!("+{refspec}")
    } else {
        refspec.to_owned()
    };
    let mut cmd = build_cmd_with_creds(path, credentials);
    cmd = cmd.arg("push");
    if set_upstream {
        cmd = cmd.arg("-u");
    }
    cmd.args([remote_name, &final_refspec]).run()?;
    if set_upstream {
        mark(&progress, "refreshing");
        let branch = refspec
            .rsplit(':')
            .next()
            .and_then(|target| target.strip_prefix("refs/heads/"))
            .unwrap_or_default();
        if !branch.is_empty() {
            build_cmd_with_creds(path, credentials)
                .args(["fetch", remote_name, branch])
                .run()?;
        }
    }
    mark(&progress, "done");
    Ok(())
}

fn do_pull(
    path: &std::path::Path,
    remote_name: &str,
    branch: &str,
    strategy: PullStrategy,
    credentials: Option<&HttpsCredentials>,
    progress: Arc<Mutex<JobProgress>>,
) -> Result<()> {
    mark(&progress, "fetching");
    // First fetch so we have the latest remote state.
    build_cmd_with_creds(path, credentials)
        .args(["fetch", remote_name])
        .run()?;

    mark(&progress, "applying");
    let remote_ref = format!("{remote_name}/{branch}");

    // Merge / rebase after fetch are local ops — no credentials needed,
    // so we go through the plain `cli::run` helper.
    let args: Vec<&str> = match strategy {
        PullStrategy::FastForwardOnly => vec!["merge", "--ff-only", &remote_ref],
        PullStrategy::Merge => vec!["merge", "--no-ff", &remote_ref],
        PullStrategy::Rebase => vec!["rebase", &remote_ref],
    };
    super::cli::run(path, &args)?;
    mark(&progress, "done");
    Ok(())
}

fn mark(p: &Arc<Mutex<JobProgress>>, stage: &str) {
    if let Ok(mut g) = p.lock() {
        g.stage = stage.to_string();
    }
}
