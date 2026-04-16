//! Shared tokio runtime.
//!
//! The egui UI loop is synchronous: every frame is a plain function
//! call. But HTTP to LLM providers is async (reqwest is built on
//! tokio). Spinning up a runtime per request is wasteful (~1ms + a
//! thread pool) and leaks threads if requests overlap, so we keep a
//! single multi-thread runtime for the whole process.
//!
//! Two entry points:
//!   * `shared()` — grab the runtime (e.g. to `spawn` a background
//!     task and await its join handle from the UI later);
//!   * `block_on(fut)` — synchronous call site, blocks the current
//!     thread until the future completes. Safe to call from egui's
//!     update loop for small/fast ops but obviously not ideal for
//!     multi-second LLM calls — prefer `spawn` + poll in those cases.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::{Builder, Runtime};

static RT: OnceLock<Runtime> = OnceLock::new();

/// Get-or-init the process-wide runtime.
///
/// We build it lazily so pure-UI invocations that never touch AI don't
/// pay the cost. `.expect` here is fine: runtime construction only
/// fails if the OS can't spawn threads, at which point nothing else
/// works either.
pub fn shared() -> &'static Runtime {
    RT.get_or_init(|| {
        Builder::new_multi_thread()
            .enable_io()
            .enable_time()
            // 2 workers is plenty for HTTP; we rarely have more than a
            // couple of concurrent AI calls in flight.
            .worker_threads(2)
            .thread_name("mergefox-ai")
            .build()
            .expect("build shared tokio runtime")
    })
}

/// Block the current thread on a future using the shared runtime.
pub fn block_on<F: Future>(f: F) -> F::Output {
    shared().block_on(f)
}

/// A single-result async job observable from the egui update loop.
///
/// Wraps a `oneshot::Receiver` so the UI can poll each frame without
/// blocking. Constructor spawns the future on the shared runtime;
/// `poll()` returns `Some(result)` exactly once when ready, `None`
/// while still running. Dropping the job aborts the spawned task on a
/// best-effort basis (it stops when the receiver hangs up — long-running
/// network calls continue until their own timeout but the result is
/// discarded, which is the intended "fire and forget" behaviour for
/// cancelled UI actions).
pub struct AiTask<T> {
    rx: tokio::sync::oneshot::Receiver<T>,
}

impl<T: Send + 'static> AiTask<T> {
    /// Spawn a future on the shared runtime and return a handle the UI
    /// can poll each frame.
    pub fn spawn<F>(fut: F) -> Self
    where
        F: Future<Output = T> + Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        shared().spawn(async move {
            // Receiver may have dropped (UI closed the modal) — ignore
            // the send error, the discarded work is acceptable.
            let _ = tx.send(fut.await);
        });
        Self { rx }
    }

    /// Non-blocking poll. Returns `Some(result)` once, then the task is
    /// "consumed" — calling `poll` again on the same handle will see the
    /// channel closed and always return `None`. Callers should drop the
    /// handle after a successful poll.
    pub fn poll(&mut self) -> Option<T> {
        use tokio::sync::oneshot::error::TryRecvError;
        match self.rx.try_recv() {
            Ok(v) => Some(v),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Closed) => None,
        }
    }
}
