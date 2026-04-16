# Release Notes

## v0.1.0-alpha.1 — First public alpha

_First public build of mergeFox. Core workflows are in place; peripheral
UI and a handful of network flows are still stabilising. Feedback is
very welcome._

### Highlights

- **Native Rust Git GUI** — no Electron, no WebView. egui + eframe with
  `glow` by default.
- **git2-free architecture** — `gix` (gitoxide) for all read-path work
  (graph walk, ref enumeration, blob loading, commit metadata) and the
  system `git` binary for every write operation. Your hooks, signing
  keys, credential helpers, proxies, and mergetools behave exactly as
  they do in a terminal.
- **Undo / redo journal** — every mutating op is recorded; `Cmd/Ctrl+Z`
  rolls back refs + working tree (with an auto-stash of any dirty
  edits).
- **Multi-tab workspaces** — open many repos at once.

### Workflows

#### Commit
- Split Unstaged / Staged panels with per-file checkboxes
- Row-level `⬇` / `⬆` to move one file; bulk `Stage selected (N)` and
  `Unstage selected (N)`
- Recent-message chips for quick re-use
- Colour-coded primary actions (`▸ Commit staged`, `Amend last`,
  `Stage all & commit`)
- Optional `✨ Generate message` against any OpenAI-compatible endpoint

#### Interactive rebase
- Tower-style planner with action dot colours (green Pick, yellow
  Reword, grey Squash, red Drop)
- Reorder arrows (▲ / ▼) and per-row action dropdowns
- Struck-through dropped rows; squash bracket (`↳`) showing target
- Bottom detail pane with **Commit** / **Changes** tabs (per-step file
  list via cached `diff_for_commit`)
- `Backup current state with tag` checkbox
- Conflicts open the resolver; after manual fixup, Continue proceeds
  through the remaining steps

#### Conflict resolver
- Operation-specific side labels — Ours / Theirs is relabeled as "HEAD
  (your branch)" vs "merging in" during a merge, and explicitly
  reversed ("Base" vs "Your change") during a rebase
- Coloured conflict-marker highlighting inside the merged-result editor
- `⬆ Prev` / `⬇ Next` region navigation via TextEdit cursor
- `⇵ Take Both` combines both sides in every region in one click
- Per-file badge showing remaining conflict count
- File list shows binary markers and conflict counts

#### Stash
- `+ Stash` button in the sidebar header — prompts for a message,
  stashes working tree + index + untracked
- Per-row right-click menu: Pop / Apply / Drop
- Double-click = pop (most common path)

#### History ops
- Context menu per commit: Checkout, Branch here, Tag here (lightweight
  or annotated), Cherry-pick, Revert, Reset (soft / mixed / hard),
  Copy SHA / short SHA, Create worktree (wire prompt), Drop / Move
  up / Move down (via the rebase planner)

#### Recovery
- Undo / redo cursor on the journal — auto-stashes dirty edits before
  ref restoration, survives process restarts
- Panic-recovery modal (`Cmd/Ctrl+Shift+Esc`) — pick any past snapshot
  and restore to a fresh `recovery-<sha>` branch
- Reflog browser with "restore to a safe branch" action

#### Forge integration
- GitHub / GitLab / Bitbucket / Gitea / Codeberg
- PAT or OAuth (device flow)
- PR creation (title / body / target branch), issue creation, sidebar
  list with selection

### Visuals

- Pastel-palette commit graph with cubic-bezier lane transitions
- Author **identicons** (GitHub-style 5×5 symmetric blocks) derived
  locally from the author email — no Gravatar round-trip, no PII leak
- HEAD chip, ref chips (local = green, remote = blue)
- Draggable column widths (Branch / Graph / Message / Author / Date /
  SHA) persisted per repo; fixed column header stays pinned while rows
  scroll

### Performance

- Graph walk runs on a background thread via gix's parallel rev-walk
- Commit rows virtualised with `ScrollArea::show_rows` — kernel-scale
  histories stay at 60 fps
- Diff pipeline collapsed into a single `git show --raw --patch` call
  (was two subprocesses per click)
- **LRU diff cache** (most recent 32) — re-clicking a commit is a
  memcpy, zero subprocesses
- **Click coalescing** — rapid clicks while a worker is running only
  ever spawn a worker for the latest selected commit; intermediate
  clicks are dropped
- Per-frame git subprocesses eliminated (conflict detection was
  spawning three `git` processes per frame — now gated by
  `.git/MERGE_HEAD` existence checks)
- Theme application memoised by hash (was resetting egui style every
  frame)
- Background workers wake the UI via `ctx.request_repaint()` the moment
  they finish so results land within one frame, not whenever the
  scheduler happens to tick
- Diff panel unified into a single `SidePanel::right("diff_panel")` id
  for both "computing…" and "loaded" states — no more layout shake on
  click

### Migration / compatibility

- `git2` / `libgit2` is no longer a dependency; the binary shrank and
  build no longer requires CMake
- All repo reads go through gix; all writes shell out to the system
  `git`
- Undo / redo journal format unchanged from previous internal builds
- `MERGEFOX_RENDERER=wgpu` still switches to the wgpu backend
  (useful on Linux Wayland where glow can be finicky)

### Known limitations

- No blame view
- No line-by-line / hunk-by-hunk stage / unstage inside the commit
  modal (file-level only for now)
- GPG signing uses your local `user.signingkey` config transparently,
  but there's no UI toggle yet
- No dedicated LFS inspector (sidebar LFS warning is informational
  only)
- Worktree creation prompt is wired but the underlying op is still a
  stub
- Some long-running network ops do not yet surface fine-grained
  progress (only a spinner + elapsed seconds)

### Environment flags

| Flag | Purpose |
|---|---|
| `MERGEFOX_RENDERER=wgpu` | Force wgpu renderer |
| `MERGEFOX_PROFILE_FRAMES=1` | Log per-frame timing to stderr |
| `MERGEFOX_PROFILE_DIFF=1` | Log `diff_for_commit` + click timing |
| `MERGEFOX_NO_AVATARS=1` | Disable author identicons |
| `MERGEFOX_STRAIGHT_LANES=1` | Use straight lane segments |
| `MERGEFOX_FORCE_CONTINUOUS=1` | Force 60 Hz rendering |
| `MERGEFOX_DISABLE_GIT_CACHE=1` | Reserved (no-op after gix migration) |

### Reporting issues

If you hit a crash or a freeze, please include:
- OS + version
- `cargo --version` / `rustc --version`
- Output of `git --version`
- A copy of the stderr log when running with
  `MERGEFOX_PROFILE_FRAMES=1 MERGEFOX_PROFILE_DIFF=1`
- Repo characteristics (approximate commit count, has submodules?,
  uses LFS?, multi-GB pack?)

---

### Acknowledgements

Built on top of exceptional open-source work:
- [`egui`](https://github.com/emilk/egui) / [`eframe`](https://github.com/emilk/egui/tree/master/crates/eframe) — immediate-mode UI toolkit
- [`gitoxide`](https://github.com/Byron/gitoxide) (`gix`) — pure-Rust Git implementation
- [`anyhow`](https://github.com/dtolnay/anyhow), [`serde`](https://github.com/serde-rs/serde), [`tokio`](https://github.com/tokio-rs/tokio) — standard Rust ecosystem workhorses
- The system [`git`](https://git-scm.com) binary itself, for every write operation
