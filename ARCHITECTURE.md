# mergeFox architecture

A map of the code for contributors and future-me. Describes the layout
as of `v0.1.0-alpha.1` + the 2026-04-17 production-readiness pass.
Updated when the shape of something shifts; file-level contents change
more often than this doc and will drift — when in doubt, trust the
module docstring over this file.

## One-paragraph overview

mergeFox is a single-binary desktop Git GUI. UI is pure Rust
(`eframe` + `egui`, glow backend by default). Read-side git operations
(graph walk, ref enumeration, blob loading) go through `gix`
(gitoxide) — pure Rust, no libgit2. Write-side operations (commit,
amend, push, pull, rebase, merge) shell out to the installed system
`git` binary so credential helpers, signing keys, hooks, and
mergetools all behave identically to running `git` in a terminal.
Background work (fetch / push / pull / long diff) runs on OS threads
via `std::thread::spawn`; async (`tokio`) is used only for the
provider REST clients and OAuth flows.

## Two backends, one reason

The read/write split is the central architectural decision and worth
calling out:

- **`gix` on the read path** — ref enumeration, commit graph,
  object loading, reflog reads. Fast, zero-alloc where it counts,
  parallel packfile resolution. We never mutate via gix.

- **System `git` on the write path** — every state change
  (`commit`, `push`, `pull`, `rebase`, `cherry-pick`, `branch -d`,
  `stash`, `worktree add`, `remote add`). Invoked through
  `src/git/cli.rs::GitCommand` with `LC_ALL=C.UTF-8`,
  `GIT_OPTIONAL_LOCKS=0`, `GIT_TERMINAL_PROMPT=0`, and
  `core.quotepath=false` set on every call.

  Why a subprocess for writes? Because the user's `~/.gitconfig`,
  credential helpers, GPG keys, aliases, hooks, and mergetools are
  all wired into the system `git` binary. Reimplementing that matrix
  inside gix would be years of work and would still lag real git.

Cloning is a special case: we try a gix fast-path first, and fall
back to the CLI if gix hits an edge case (see `src/clone.rs`).

## Module map

```
src/
├── main.rs                  eframe entry, renderer selection, log init
├── app.rs                   MergeFoxApp — top-level state; workspace tabs,
│                            pending prompt, settings modal, HUD, panics
├── actions.rs               CommitAction enum — every user-triggered op
│                            goes through this so journal/MCP/palette
│                            can hook one dispatcher
├── preflight.rs             Destructive-op preview: "this will drop
│                            N commits, tree is dirty, …"
├── config.rs                config.json schema, recents, theme, repo prefs
├── secrets.rs               Internal SecretStore (~/mergefox/secrets.json
│                            chmod 0600) — replaced the OS keychain dep
├── journal/                 Undo/redo log (file-backed append)
├── logging.rs               tracing init + rolling daily file
├── clone.rs                 Clone orchestration (gix → cli fallback)
├── gix_clone.rs             gix-specific clone with progress-tree watcher
├── git_url.rs               Git URL parsing (https, ssh, shorthand)
├── forge.rs                 "Forge" = remote provider (GitHub / GitLab / …)
│                            state on a workspace
│
├── git/                     ───────── git layer
│   ├── cli.rs               GitCommand builder, subprocess runner,
│   │                        log ring, error classifier
│   ├── repo.rs              Repo handle (gix) + read methods
│   ├── diff.rs              git show → FileDiff parsing
│   ├── graph.rs             Commit graph walk + ref label collection
│   ├── ops.rs               stage/unstage/commit/amend/stash/status
│   ├── jobs.rs              Background fetch/push/pull with progress
│   ├── lfs.rs               Git-LFS candidate scan
│   └── mod.rs               Re-exports
│
├── ui/                      ───────── egui views
│   ├── mod.rs               Section registration
│   ├── theme.rs             Palette + visuals + density tuning
│   ├── fonts.rs             Compile-time subset + CJK fallback
│   ├── welcome.rs           First-run / no-repo-open view
│   ├── top_bar.rs           Title bar + fetch/push/pull/settings buttons
│   ├── sidebar.rs           Branches + stashes + LFS scan
│   ├── main_panel.rs        Graph + diff + dispatcher entry
│   ├── graph.rs             Commit graph renderer + context menu
│   ├── diff_view.rs         Text + image diff viewer
│   ├── commit_modal.rs      Commit message input + AI ✨ generate
│   ├── conflicts.rs         Merge conflict resolution
│   ├── rebase.rs            Interactive rebase (alpha scope: drop/reorder)
│   ├── reflog.rs            Recovery window (⌘⇧R)
│   ├── prompt.rs            Confirmation + text-input modals
│   ├── panic_recovery.rs    Modal shown after a panic
│   ├── hud.rs               Bottom-strip transient notifications
│   ├── forge.rs             Provider-side (PR / issues) view
│   ├── publish_remote.rs    "Create hosted repo from this local checkout"
│   ├── activity_log.rs      MCP activity log inspector
│   ├── tabs.rs              Workspace tabs
│   ├── columns.rs           Graph column visibility
│   ├── icon.rs              App icon loader
│   ├── syntax.rs            Diff syntax highlighting
│   └── settings/            ───── Settings window
│       ├── mod.rs           Window shell + sidebar + section dispatch
│       ├── general.rs       Language + theme
│       ├── repo.rs          Default remote + pull strategy + remote
│       │                    CRUD + worktree list
│       ├── integrations.rs  Provider accounts (PAT / OAuth / SSH)
│       └── ai.rs            AI endpoint config + test probe
│
├── providers/               ───────── Remote hosting providers
│   ├── mod.rs               Provider trait, AccountId, RemoteRepoSummary
│   ├── github.rs            GitHub REST client
│   ├── gitlab.rs            GitLab REST client
│   ├── bitbucket.rs         Bitbucket REST client
│   ├── azure.rs             Azure DevOps REST client
│   ├── gitea.rs             Gitea / Codeberg REST client
│   ├── generic.rs           Fallback for "git server with no API"
│   ├── oauth.rs             Device-code flow
│   ├── pat.rs               PAT verification
│   ├── ssh.rs               ed25519 keygen + ssh-agent registration
│   ├── runtime.rs           tokio runtime wrapper for background tasks
│   └── types.rs             Shared request/response types
│
├── ai/                      ───────── AI task runner
│   ├── mod.rs               Endpoint config, API key loader
│   ├── client.rs            HTTP client (reqwest → Ollama/OpenAI/Anthropic)
│   ├── error.rs             AiError
│   ├── runtime.rs           AiTask<T>: spawn + poll() each frame
│   ├── config.rs            Endpoint → keyring bridge
│   └── tasks/
│       ├── commit_message.rs     Generate commit message from diff
│       ├── commit_composer.rs    Split staged diff into logical commits
│       ├── explain_change.rs     Markdown-formatted diff explanation
│       ├── pr_conflict.rs        Suggest merge resolution for conflict
│       └── stash_message.rs      Generate stash message
│
└── mcp/                     ───────── Agent gateway (in-process for now)
    └── mod.rs               ActivityLog + export
```

## Data flow for a user action

Take "delete branch" as a representative example — it's destructive,
goes through a prompt, and touches the journal.

```
┌──────────────────────────────────────────────────────────────────┐
│ 1. User right-clicks a branch label in ui::graph::render_commit_menu │
│    → returns Some(CommitAction::DeleteBranchPrompt { name, is_remote }) │
└─────────────┬────────────────────────────────────────────────────┘
              │
              ▼
┌──────────────────────────────────────────────────────────────────┐
│ 2. ui::main_panel::run_action matches DeleteBranchPrompt:        │
│    - preflight::delete_branch(repo_path, name, is_remote)        │
│      counts unreachable commits via `git log branch --not ...`   │
│    - Builds PendingPrompt::Confirm { kind: DeleteBranch, preflight } │
│    - Returns DispatchOutcome { prompt: Some(p), .. }             │
└─────────────┬────────────────────────────────────────────────────┘
              │
              ▼
┌──────────────────────────────────────────────────────────────────┐
│ 3. ui::prompt::show renders the modal with preflight lines       │
│    ("⛔ 3 commits exist only on `branch` …"). User clicks Delete. │
│    Modal flips confirmed=true, then calls main_panel::dispatch_prompt. │
└─────────────┬────────────────────────────────────────────────────┘
              │
              ▼
┌──────────────────────────────────────────────────────────────────┐
│ 4. ui::main_panel::run_prompt matches ConfirmKind::DeleteBranch: │
│    - journal::capture(repo_path) → RepoSnapshot (before)         │
│    - ws.repo.delete_branch(name, is_remote)                      │
│      → git::cli::run(["branch", "-d", name]) subprocess          │
│    - journal::capture(repo_path) → RepoSnapshot (after)          │
│    - DispatchOutcome.journal_entry = (Operation::DeleteBranch, before, after) │
└─────────────┬────────────────────────────────────────────────────┘
              │
              ▼
┌──────────────────────────────────────────────────────────────────┐
│ 5. apply_outcome wires outcome back into the app:                │
│    - app.journal_record(op, before, after) → append to journal file │
│    - app.hud = "Deleted branch foo" (transient)                  │
│    - app.rebuild_graph(scope) → graph re-walk on next frame      │
└──────────────────────────────────────────────────────────────────┘
```

Every destructive / mutating action follows the same pipeline:
`CommitAction → dispatcher → git op (CLI subprocess) → journal entry
→ outcome application`. Read-only actions (copy SHA, checkout
navigation, fetch) skip the journal.

## Background work

Three mechanisms, picked per task:

1. **Thread + channel**, for git jobs that need cancel + progress:
   `src/git/jobs.rs::GitJob::spawn` starts a `std::thread`, emits
   progress into `Arc<Mutex<JobProgress>>`, honors a cancel
   `AtomicBool`, returns the result through an `mpsc::channel`. The
   UI polls `.poll()` and `.snapshot()` each frame. Fetch / push /
   pull use this; clone uses a similar but separate path
   (`src/clone.rs`).

2. **`AiTask<T>`**, for AI endpoint probes: tokio `oneshot` future
   running on a shared tokio runtime; polled each UI frame. Used in
   `Settings → AI → Test`, commit message generation, etc.

3. **Provider runtime**, for OAuth / REST probes: same shape as
   `AiTask`, separate runtime so AI and provider work don't starve
   each other. Lives in `src/providers/runtime.rs`.

The UI **never** blocks on a subprocess or HTTP call. Every call
that could take longer than a frame goes through one of the three.

## Persistence layout

| Kind | Path (macOS) | Notes |
|---|---|---|
| Config | `~/Library/Application Support/mergefox/config.json` | JSON, schema versioned (`SCHEMA_VERSION`) |
| Secrets | `~/Library/Application Support/mergefox/secrets.json` | chmod 0600, replaces OS keychain |
| Journal | `.git/mergefox/journal.log` (per repo) | Append-only, undo/redo reads this |
| Logs | `~/Library/Logs/mergefox/mergefox.log.YYYY-MM-DD` | `tracing-appender` daily rotation |
| Auto-stash | `refs/mergefox/autostash-*` (per repo) | 7-day retention via `prune_autostashes` |

Linux uses `$XDG_CONFIG_HOME`, `$XDG_STATE_HOME`. Windows uses
`%APPDATA%` / `%LOCALAPPDATA%`. All three OS paths are resolved via
the `dirs` crate plus small OS-specific overrides in
`logging::log_dir`.

## Extension points

Where new features tend to land:

| You want to add… | Start from |
|---|---|
| A new command-menu entry | `CommitAction` enum + `ui::graph::render_commit_menu` |
| A destructive op with a pre-flight warning | Add a compute fn in `preflight.rs`, wire into the matching `ConfirmKind` in `prompt.rs` |
| A new background git job kind | `GitJobKind` enum + match arm in `run_job` |
| A new settings section | Add a variant to `SettingsSection`, a module under `ui/settings/`, and a dispatch arm in `ui::settings::mod::render_body` |
| A new hosting provider | Implement the `Provider` trait in a new file under `providers/` |
| A new AI task | Add a file under `ai/tasks/`, expose via `ai::tasks::mod` |

## Known design debts

- **`CommitAction` is a flat enum, not a trait.** Fine today; will
  start hurting once the command palette, MCP, and multi-select all
  need the same metadata (preconditions, preview, undo). Tracked as
  `TODO/production.md` §F1.
- **Dispatcher lives inside `ui::main_panel`** rather than its own
  module. History reason (it grew out of the graph view).
  Reachable from many sites, should be promoted.
- **Settings modal keeps a lot of transient state.** `SettingsModal`
  struct in `app.rs` has ~20 fields. A per-section state extraction
  would reduce churn when new sections land.
- **No test fixture for integration.** Every test is in-process
  parsing or pure-function. `TODO/production.md` §B1.
