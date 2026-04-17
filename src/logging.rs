//! Logging / tracing bootstrap.
//!
//! Why a dedicated module (vs. calling the `tracing_subscriber` API inline
//! from `main`): log output in a desktop app has to go **three** places —
//! stderr during development, a rolling file for bug reports, and
//! (optionally) JSON for machine-readable diagnostics. Centralizing the
//! policy here keeps the init 3 lines long at the call site.
//!
//! The env var surface:
//!   * `MERGEFOX_LOG`         — `tracing-subscriber` EnvFilter directive.
//!                              Default: `info,mergefox=debug` in debug
//!                              builds, `warn,mergefox=info` in release.
//!   * `MERGEFOX_LOG_FORMAT`  — `text` (default) or `json`. JSON is used
//!                              when attaching logs to bug reports.
//!   * `MERGEFOX_LOG_STDERR`  — `0` to suppress stderr output (file only).
//!
//! Files are rolled daily under the OS log directory:
//!   * macOS   `~/Library/Logs/mergefox/`
//!   * Linux   `$XDG_STATE_HOME/mergefox/` (or `~/.local/state/mergefox/`)
//!   * Windows `%LOCALAPPDATA%\mergefox\logs\`

use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

/// Handle returned from `init`. Must be kept alive for the process
/// lifetime — dropping it flushes any buffered file writes, and we
/// don't want that to happen mid-log.
pub struct LogGuard {
    _file: Option<WorkerGuard>,
}

/// Initialize global tracing. Call once from `main`, capture the returned
/// `LogGuard`, and hold it until the process exits.
pub fn init() -> LogGuard {
    let filter = EnvFilter::try_from_env("MERGEFOX_LOG")
        .unwrap_or_else(|_| EnvFilter::new(default_filter()));

    let stderr_enabled = std::env::var("MERGEFOX_LOG_STDERR")
        .map(|v| v != "0")
        .unwrap_or(true);

    let json = std::env::var("MERGEFOX_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    // File appender — never block the UI thread. If creating the log
    // directory fails we fall back to stderr-only; better to lose file
    // logs than to crash on startup.
    let (file_layer, file_guard) = match log_dir() {
        Some(dir) => match std::fs::create_dir_all(&dir) {
            Ok(()) => {
                let appender = tracing_appender::rolling::daily(&dir, "mergefox.log");
                let (nb, guard) = tracing_appender::non_blocking(appender);
                let layer = fmt::layer()
                    .with_writer(nb)
                    .with_ansi(false)
                    .with_target(true)
                    .with_thread_ids(false)
                    .with_file(false)
                    .with_line_number(false);
                (Some(layer.boxed()), Some(guard))
            }
            Err(_) => (None, None),
        },
        None => (None, None),
    };

    let stderr_layer = if stderr_enabled {
        if json {
            Some(fmt::layer().json().with_writer(std::io::stderr).boxed())
        } else {
            Some(
                fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_ansi(atty_stderr())
                    .with_target(false)
                    .boxed(),
            )
        }
    } else {
        None
    };

    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer);

    // Allow re-init to be a no-op rather than panic (useful for tests that
    // call `init` indirectly via app helpers).
    let _ = subscriber.try_init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        target_os = std::env::consts::OS,
        target_arch = std::env::consts::ARCH,
        "mergefox starting"
    );

    LogGuard { _file: file_guard }
}

fn default_filter() -> String {
    if cfg!(debug_assertions) {
        format!("info,mergefox=debug")
    } else {
        format!("warn,mergefox=info")
    }
}

fn atty_stderr() -> bool {
    // Minimal heuristic: if stderr is redirected we skip ANSI. Avoids
    // a dependency on the `atty` crate for one boolean.
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

/// Resolve the OS-appropriate log directory. Returns `None` only on
/// exotic platforms where `dirs::state_dir` and friends all fail.
pub fn log_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = dirs::home_dir() {
            return Some(home.join("Library").join("Logs").join("mergefox"));
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = dirs::data_local_dir() {
            return Some(local.join("mergefox").join("logs"));
        }
    }
    // Linux / BSD: prefer `$XDG_STATE_HOME`, fall back to the data dir.
    if let Some(state) = dirs::state_dir() {
        return Some(state.join("mergefox"));
    }
    dirs::data_dir().map(|d| d.join("mergefox").join("logs"))
}
