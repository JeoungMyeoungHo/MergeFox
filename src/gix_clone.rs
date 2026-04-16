//! Gitoxide-backed clone path.
//!
//! Why gitoxide and not libgit2?
//! ------------------------------
//! On large repositories (kernel-scale, ~11M objects) libgit2 was
//! single-threaded for `index-pack` and its protocol-v2 shallow support
//! in the 1.7 release vendored by git2 0.18 is buggy — shallow clones of
//! GitHub freeze at "connecting" because libgit2 never completes the
//! initial capability negotiation. Gitoxide (`gix`) has:
//!
//!   * **Parallel pack resolution** — index-pack runs with all CPU cores,
//!     typically 3–5× faster than libgit2 on huge packs.
//!   * **Correct protocol-v2 shallow** — `Shallow::DepthAtRemote` works
//!     against GitHub/GitLab/etc. without the "connecting" hang.
//!   * **Lower resident memory** — streaming architecture instead of
//!     libgit2's load-everything model.
//!
//! Progress bridging
//! -----------------
//! gix reports progress through a `prodash` tree: every operation
//! (negotiation, receive pack, index pack, resolve deltas, checkout) is
//! a node with a step/done_at counter. We spawn a lightweight poller
//! that walks the root tree every 150 ms, picks the deepest task with a
//! known total, and writes its numbers into our shared `CloneProgress`.
//! That's what the welcome-screen progress bar and top-bar spinner read.

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use gix::progress::prodash::tree::Root;
// Task/Value are re-exported at `gix::progress::*` (from `gix-features`).
use gix::progress::{Task, Value};

use crate::clone::{CloneProgress, Stage};

/// Clone `url` into `dest` using gitoxide. `depth`, when set, produces a
/// shallow clone with that many commits on each tip.
///
/// Blocks the calling thread until the clone completes — intended to run
/// inside the same background thread as the libgit2 path.
pub fn do_clone_gix(
    url: &str,
    dest: &Path,
    depth: Option<u32>,
    progress: Arc<Mutex<CloneProgress>>,
) -> Result<()> {
    mark_stage(&progress, Stage::Connecting);

    // Parse first so we surface "bad URL" as a Result rather than a panic
    // inside gix.
    let parsed_url = gix::url::parse(url.as_bytes().into())
        .with_context(|| format!("parse clone URL {url}"))?;

    // Prodash tree. Handed to the cloner as the progress sink, and
    // cloned-by-reference into the poller thread. Item is taken as the
    // top-level progress node (gix adds children below it as it works).
    let root = Root::new();
    let clone_item = root.add_child("mergefox-clone");

    // Interrupt flag: a future cancel button would flip this to `true`;
    // for now it's always `false`.
    let interrupt = Arc::new(AtomicBool::new(false));

    // Kick off progress polling on a side thread. It mirrors the
    // deepest-active-task's numbers into our CloneProgress so the UI
    // progress bar moves smoothly instead of sitting at 0/0 until the
    // whole clone is done.
    let stop = Arc::new(AtomicBool::new(false));
    let poller = {
        let root_for_poll = root.clone();
        let progress_for_poll = progress.clone();
        let stop_for_poll = stop.clone();
        thread::spawn(move || poll_progress(root_for_poll, progress_for_poll, stop_for_poll))
    };

    let clone_result = run_clone(parsed_url, dest, depth, clone_item, &interrupt);

    stop.store(true, Ordering::Relaxed);
    let _ = poller.join();

    clone_result?;
    mark_completion(&progress);
    Ok(())
}

fn run_clone(
    url: gix::url::Url,
    dest: &Path,
    depth: Option<u32>,
    mut progress: gix::progress::prodash::tree::Item,
    interrupt: &Arc<AtomicBool>,
) -> Result<()> {
    use gix::remote::fetch::Shallow;

    let mut prepare = gix::clone::PrepareFetch::new(
        url,
        dest,
        gix::create::Kind::WithWorktree,
        gix::create::Options::default(),
        gix::open::Options::default(),
    )
    .context("gix clone init")?;

    if let Some(d) = depth {
        let d = NonZeroU32::new(d.max(1)).expect("depth.max(1) >= 1");
        prepare = prepare.with_shallow(Shallow::DepthAtRemote(d));
    }

    let (mut prepare_checkout, _outcome) = prepare
        .fetch_then_checkout(&mut progress, interrupt)
        .context("gix fetch")?;

    let (_repo, _outcome) = prepare_checkout
        .main_worktree(&mut progress, interrupt)
        .context("gix checkout")?;

    Ok(())
}

fn mark_stage(progress: &Arc<Mutex<CloneProgress>>, stage: Stage) {
    if let Ok(mut g) = progress.lock() {
        g.stage = stage;
    }
}

fn mark_completion(progress: &Arc<Mutex<CloneProgress>>) {
    if let Ok(mut g) = progress.lock() {
        g.stage = Stage::Checkout;
        if g.total_objects == 0 {
            g.total_objects = g.received_objects.max(1);
        }
        g.received_objects = g.total_objects;
    }
}

/// Walk the prodash tree periodically and surface the most relevant
/// numbers to the shared `CloneProgress`.
///
/// "Most relevant" = the task with the largest `done_at` that has at
/// least some progress (`step > 0`). A deeper subtask typically has a
/// concrete total (bytes to receive, objects to index, etc.) while the
/// root-level "mergefox-clone" node stays at 0/0 the whole time — if we
/// surfaced that, the UI would look frozen. Picking the busiest concrete
/// subtask is the heuristic git's own CLI uses for the same reason.
fn poll_progress(
    root: Arc<Root>,
    progress: Arc<Mutex<CloneProgress>>,
    stop: Arc<AtomicBool>,
) {
    // `sorted_snapshot` fills a `Vec<(Key, Task)>` but we don't care
    // about the key type — let type inference pick the correct Key
    // impl from prodash's private tree module.
    let mut snapshot = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        snapshot.clear();
        root.sorted_snapshot(&mut snapshot);

        // Find the task with the highest `done_at` that has actual progress.
        let mut best: Option<(String, usize, usize)> = None;
        for (_key, task) in &snapshot {
            let Some(v) = task.progress.as_ref() else {
                continue;
            };
            let Value {
                ref step,
                done_at,
                ..
            } = *v;
            let step_now = step.load(Ordering::Relaxed);
            if step_now == 0 {
                continue;
            }
            let total = done_at.unwrap_or(step_now);
            // Prefer tasks with the most work measured (largest total)
            // so the UI always shows the biggest visible bar instead of
            // ping-ponging between a short checkout and a long fetch.
            match &best {
                Some((_, _, best_total)) if total <= *best_total => {}
                _ => {
                    best = Some((task.name.clone(), step_now, total));
                }
            }
        }

        if let Some((name, step_now, total)) = best {
            if let Ok(mut g) = progress.lock() {
                g.received_objects = step_now;
                g.total_objects = total;
                g.stage = classify_stage(&name);
            }
        }

        thread::sleep(Duration::from_millis(150));
    }
}

fn classify_stage(task_name: &str) -> Stage {
    // Heuristic name matching. gix task names are stable enough for this,
    // and a misclassification only mis-labels the UI — it doesn't affect
    // correctness.
    let n = task_name.to_ascii_lowercase();
    if n.contains("checkout") || n.contains("work tree") || n.contains("worktree") {
        Stage::Checkout
    } else if n.contains("resolv") || n.contains("decode") || n.contains("index pack") {
        Stage::Resolving
    } else if n.contains("receiv") || n.contains("fetch") || n.contains("bytes") {
        Stage::Receiving
    } else {
        Stage::Connecting
    }
}
