//! Shared tokio runtime for synchronous UI callers.
//!
//! egui runs on the main thread and is fundamentally synchronous; provider
//! operations are `async fn`. Rather than spin up a fresh runtime per call
//! (slow, racy) or force every caller to manage its own, we host a single
//! multi-thread runtime behind a `OnceLock`.
//!
//! Call `runtime::shared().block_on(fut)` from a background worker thread
//! (NOT from the UI thread — that would freeze the window during network I/O).

use std::sync::OnceLock;

use tokio::runtime::{Builder, Runtime};

static RT: OnceLock<Runtime> = OnceLock::new();

pub fn shared() -> &'static Runtime {
    RT.get_or_init(|| {
        // Named threads help when inspecting stack traces / profilers —
        // it's obvious which threads belong to provider work.
        Builder::new_multi_thread()
            .enable_all()
            .thread_name("mergefox-provider")
            .worker_threads(2)
            .build()
            .expect("build provider tokio runtime")
    })
}

pub struct ProviderTask<T> {
    rx: tokio::sync::oneshot::Receiver<T>,
}

impl<T: Send + 'static> ProviderTask<T> {
    pub fn spawn<F>(fut: F) -> Self
    where
        F: std::future::Future<Output = T> + Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        shared().spawn(async move {
            let _ = tx.send(fut.await);
        });
        Self { rx }
    }

    pub fn poll(&mut self) -> Option<T> {
        use tokio::sync::oneshot::error::TryRecvError;
        match self.rx.try_recv() {
            Ok(v) => Some(v),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Closed) => None,
        }
    }
}
