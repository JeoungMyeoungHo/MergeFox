//! Thin wrapper around the system `git` binary.
//!
//! Write-path git operations (commit, amend, checkout, rebase, stash,
//! cherry-pick, revert, reset, branch CRUD, …) are delegated to the
//! installed `git` executable instead of libgit2 or gix. Rationale:
//!
//!   * libgit2 reimplements these in C with its own quirks around
//!     config precedence, hook invocation, and conflict markers — at
//!     mergeFox's scale we don't want to maintain compatibility with
//!     two mental models.
//!   * gix's write path is still maturing; its `commit` / `rebase` /
//!     `stash` APIs either don't exist or lack parity with real git.
//!   * Users' local `git` binary already respects their global config,
//!     hooks (`pre-commit`, `commit-msg`, `post-merge`, …), signing
//!     keys, credential helpers, and custom mergetools. Shelling out
//!     means a mergeFox commit behaves EXACTLY like a terminal commit,
//!     which is the whole point of a "lightweight git GUI".
//!
//! Read-path operations (ref enumeration, status, graph walk, diff)
//! stay in-process via gix so we don't pay a `Command::spawn` per
//! UI paint.
//!
//! ## Safety / hygiene
//!
//! * Always pass `--` before path arguments when the caller supplies
//!   user-controlled paths, so a path starting with `-` is treated as
//!   a path and not a flag.
//! * Stderr is captured and included in error messages so users can
//!   see the real git diagnostic ("not a valid object name", etc.)
//!   instead of a generic `exit code 128`.
//! * `GIT_OPTIONAL_LOCKS=0` is set by default to keep passive UI
//!   queries from racing with user terminal activity (git maintenance
//!   lock contention was visible on status polls).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};

/// A single entry in the git command log.
#[derive(Debug, Clone)]
pub struct GitLogEntry {
    /// Wall-clock unix timestamp (seconds).
    pub timestamp: i64,
    /// Working directory the command ran in.
    pub cwd: String,
    /// The full argument list (excluding the implicit `--no-pager` etc).
    pub args: Vec<String>,
    /// Exit code (`0` = success).
    pub exit_code: i32,
    /// How long the subprocess took.
    pub duration_ms: u64,
    /// First ~200 chars of stderr (for error diagnosis).
    pub stderr_head: String,
}

/// Process-wide ring buffer of recent git commands. Readable from
/// any thread (the UI's "Git Log" panel consumes it); written to
/// at the end of every `GitCommand::run_raw`.
static GIT_LOG: Mutex<Option<GitCommandLog>> = Mutex::new(None);

struct GitCommandLog {
    entries: std::collections::VecDeque<GitLogEntry>,
    capacity: usize,
}

impl GitCommandLog {
    fn new(capacity: usize) -> Self {
        Self {
            entries: std::collections::VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, entry: GitLogEntry) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }
}

fn log_entry(entry: GitLogEntry) {
    // Optionally print to stderr for CLI-level debugging.
    if std::env::var("MERGEFOX_LOG_GIT").is_ok() {
        let status = if entry.exit_code == 0 {
            "ok".to_string()
        } else {
            format!("exit {}", entry.exit_code)
        };
        eprintln!(
            "[git {}ms {}] git {}{}",
            entry.duration_ms,
            status,
            entry.args.join(" "),
            if entry.stderr_head.is_empty() {
                String::new()
            } else {
                format!("  stderr: {}", entry.stderr_head)
            },
        );
    }
    let mut guard = GIT_LOG.lock().unwrap_or_else(|e| e.into_inner());
    let log = guard.get_or_insert_with(|| GitCommandLog::new(200));
    log.push(entry);
}

/// Read a snapshot of the recent git command log. The UI calls this
/// to populate the "Git Log" panel.
pub fn recent_git_log() -> Vec<GitLogEntry> {
    let guard = GIT_LOG.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(log) => log.entries.iter().cloned().collect(),
        None => Vec::new(),
    }
}

/// Result of a `git` invocation. Success implies exit code 0.
#[derive(Debug, Clone)]
pub struct CliOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: i32,
}

impl CliOutput {
    /// Stdout decoded as UTF-8 lossily. Most git plumbing commands
    /// emit UTF-8; porcelain commands honour `core.quotepath=false`
    /// via our default env so non-ASCII paths come through unquoted.
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Builder for a single git invocation. Prefer the convenience helpers
/// (`run`, `run_with_stdin`) below; construct this directly only when
/// you need to customise stdin / env beyond the defaults.
pub struct GitCommand {
    repo_path: PathBuf,
    args: Vec<OsString>,
    stdin: Option<Vec<u8>>,
    env: Vec<(OsString, OsString)>,
}

impl GitCommand {
    pub fn new(repo_path: &Path) -> Self {
        Self {
            repo_path: repo_path.to_path_buf(),
            args: Vec::new(),
            stdin: None,
            env: Vec::new(),
        }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for a in args {
            self.args.push(a.as_ref().to_os_string());
        }
        self
    }

    /// Pipe `data` to git's stdin. Used by `commit -F -`, `hash-object
    /// --stdin`, etc., to avoid shell-escaping commit messages with
    /// newlines.
    pub fn stdin(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    pub fn env(mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> Self {
        self.env
            .push((key.as_ref().to_os_string(), val.as_ref().to_os_string()));
        self
    }

    /// Execute the command, collecting stdout/stderr. Returns
    /// `CliOutput` on exit-zero; returns Err containing stderr on
    /// non-zero exit so callers don't need to remember to check.
    pub fn run(self) -> Result<CliOutput> {
        let output = self.run_raw()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let code = output.status.code().unwrap_or(-1);
            bail!(
                "git exited with code {code}: {}{}",
                stderr.trim(),
                if stdout.trim().is_empty() {
                    String::new()
                } else {
                    format!(" (stdout: {})", stdout.trim())
                }
            );
        }
        Ok(CliOutput {
            stdout: output.stdout,
            stderr: output.stderr,
            status: output.status.code().unwrap_or(0),
        })
    }

    /// Like `run` but returns the raw `Output` regardless of exit
    /// status. Use for commands where non-zero exit is a meaningful
    /// signal (e.g. `git merge-base --is-ancestor` returning 1 for
    /// "not an ancestor").
    pub fn run_raw(self) -> Result<Output> {
        let GitCommand {
            repo_path,
            args,
            stdin,
            env,
        } = self;
        let t0 = Instant::now();
        let args_snapshot: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let cwd_snapshot = repo_path.display().to_string();
        let mut cmd = Command::new("git");
        cmd.current_dir(&repo_path)
            // `--no-pager` so help-like commands don't hang on TTY
            // detection; `-c core.quotepath=false` so path bytes come
            // through unescaped for display purposes.
            .args(["--no-pager", "-c", "core.quotepath=false"])
            .args(&args)
            // Prevent passive UI queries from fighting with user
            // terminal activity for repo locks (especially maintenance
            // `.lock` files during background fetch / repack).
            .env("GIT_OPTIONAL_LOCKS", "0")
            // Don't let system locale spin git's output encoding into
            // something exotic — our parsers assume UTF-8.
            .env("LC_ALL", "C.UTF-8")
            // We're a GUI app with no TTY attached. Without this, any
            // subprocess that hits "need credentials" (HTTPS push to a
            // private repo without a credential helper configured) would
            // block forever waiting on a username prompt that can never
            // be answered — user sees the spinner hang with no explanation.
            // `=0` makes git fail fast with an actionable error instead.
            // When credentials are known (e.g. OAuth token for a
            // connected provider) we override this per-call in `jobs.rs`
            // by injecting a `credential.helper=!…` inline script.
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if stdin.is_some() {
            cmd.stdin(Stdio::piped());
        }
        for (k, v) in env {
            cmd.env(k, v);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn git ({} in {:?})", summarize(&args), repo_path))?;
        if let Some(data) = stdin {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .ok_or_else(|| anyhow!("git stdin pipe not opened"))?
                .write_all(&data)
                .context("write to git stdin")?;
        }
        let output = child
            .wait_with_output()
            .with_context(|| format!("wait for git ({})", summarize(&args)))?;

        // Log the command + outcome.
        let stderr_bytes = &output.stderr;
        let stderr_head = String::from_utf8_lossy(
            &stderr_bytes[..stderr_bytes.len().min(200)],
        )
        .trim()
        .to_string();
        log_entry(GitLogEntry {
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            cwd: cwd_snapshot,
            args: args_snapshot,
            exit_code: output.status.code().unwrap_or(-1),
            duration_ms: t0.elapsed().as_millis() as u64,
            stderr_head,
        });

        Ok(output)
    }
}

/// One-liner convenience: `run(path, ["status", "--porcelain"])`.
pub fn run<I, S>(repo_path: &Path, args: I) -> Result<CliOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    GitCommand::new(repo_path).args(args).run()
}

/// Run and return stdout trimmed, suitable for commands that yield a
/// single line (`rev-parse HEAD`, `config --get user.name`, …).
pub fn run_line<I, S>(repo_path: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = run(repo_path, args)?;
    Ok(out.stdout_str().trim().to_owned())
}

fn summarize(args: &[OsString]) -> String {
    args.iter()
        .take(3)
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}
