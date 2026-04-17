# Contributing to mergeFox

Thanks for taking the time to look at this. The project is in **alpha**
(`v0.1.0-alpha.x`), which means everything below is still moving — if
anything here contradicts reality, the reality wins and the docs are
wrong. Open an issue or a quick PR.

## Ground rules

1. **Ask before big work.** Anything over ~300 LOC or that touches more
   than 3 files, please open an issue first so we can sanity-check
   scope before either of us invests time.
2. **The alpha is feature-hungry but quality-conservative.** We would
   rather ship five well-made features than fifty half-done ones. A PR
   that adds a button nobody tested is worse than no PR.
3. **Destructive ops need a safety net.** Anything that can lose work
   (reset, rebase, force-push, drop, delete-branch) must go through the
   `preflight` module and the confirmation modal with concrete numbers.
   No silent `--force`.
4. **The system `git` binary is the write path.** Read path is `gix`.
   If you catch yourself reaching for libgit2 / git2-rs / a third
   backend, stop — discuss first. We deliberately have two backends, not
   three.

## Development setup

### Prerequisites

- Rust **1.76** or newer (`rust-version` in `Cargo.toml`). `rustup`
  will pick the right toolchain automatically.
- A working `git` on your `PATH` (`git --version` ≥ 2.30 recommended).
  mergeFox shells out to it for writes.
- Platform build deps (egui / eframe stack):
  - **Linux**: `libgtk-3-dev libxcb-render0-dev libxcb-shape0-dev
    libxcb-xfixes0-dev libxkbcommon-dev libssl-dev libasound2-dev
    libfontconfig1-dev`
  - **macOS**: Xcode Command Line Tools only. No extra brews needed.
  - **Windows**: the MSVC toolchain (`rustup default stable-msvc`).

### First build

```bash
git clone https://github.com/JeoungMyeoungHo/MergeFox.git
cd MergeFox
cargo build              # debug, warm build ~30s on M-series
./target/debug/mergefox
```

### Release build

```bash
cargo build --release
./target/release/mergefox
```

Release builds take ~1m30s on first compile and ~20s incrementally.
`lto = "fat"` + `codegen-units = 1` are on by default — turn them off
in your local `Cargo.toml` (or a `[profile.release-dev]`) if you need
faster release iteration.

## Code layout

Short version; see [`ARCHITECTURE.md`](./ARCHITECTURE.md) for the long
one.

```
src/
  main.rs               eframe entry point, renderer selection
  app.rs                MergeFoxApp — the top-level state holder
  actions.rs            CommitAction enum — every user-triggered op
  preflight.rs          Pre-flight info for destructive ops (F4)
  logging.rs            tracing init + file rotation

  git/                  System-git wrapper + gix read path + jobs
  ui/                   egui views (graph, sidebar, commit modal, …)
  ui/settings/          Settings window sections
  providers/            GitHub / GitLab / Bitbucket / … REST clients
  ai/                   AI task runner + endpoint config
  mcp/                  In-process MCP activity log
  journal/              Undo/redo journal (file-backed)
```

## What's in `TODO/`

| File | What it tracks |
|---|---|
| [`TODO/features.md`](./TODO/features.md) | Feature gaps — push modal, stash sidebar, provider UI, AI wiring, MCP transport, tech debt |
| [`TODO/production.md`](./TODO/production.md) | Infra & UX polish — CI, signing, logging, pre-flight, keyboard nav, command palette, … |

New work that needs tracking should go in whichever file it matches
better. If in doubt, `production.md` for infra / cross-cutting, and
`features.md` for a concrete user-visible feature.

## Running the tests

```bash
cargo test                          # all tests
cargo test --bin mergefox worktree  # only worktree parser tests
```

Most of the test suite is unit-level: CLI error classification, URL
parsing, worktree porcelain parsing, AI JSON parsing, commit author
normalization. There is **no** repo-fixture integration test yet
(tracked as `TODO/production.md` §B1).

## Style

- `cargo fmt` before committing. CI runs `fmt --check` as advisory
  during alpha; strict mode comes later.
- `cargo clippy` is also advisory right now (~150 pre-existing
  dead-code warnings from scaffolded modules, tracked as
  `TODO/features.md` §6.2). Please don't add *new* clippy warnings.
- Every `unsafe` block needs a comment explaining why it's safe.
- `println!` / `eprintln!` are banned in non-test code — use
  `tracing::{debug, info, warn, error}`. The `MERGEFOX_LOG` env var
  controls filtering at runtime.

## Commit messages

We follow a loose Conventional Commits style:

```
type(scope): one-line summary

Longer explanation — why, not what. The diff explains the what.
Cite related code paths inline so future archaeologists can grep.

Co-Authored-By: <your name> <you@example.com>
```

Types we use: `feat`, `fix`, `chore`, `docs`, `refactor`, `perf`,
`test`. Scope is a short subsystem name (`ui`, `git`, `providers`,
`infra`, `graph`, …). 72-char summary limit is a soft guideline — a
clean 80-char summary is better than a truncated 72.

## Reporting bugs

File an issue with:

1. Platform + OS version + `mergefox --version` (or commit sha if you
   built from source).
2. The steps to reproduce.
3. Relevant log lines from:
   - macOS: `~/Library/Logs/mergefox/mergefox.log.*`
   - Linux: `$XDG_STATE_HOME/mergefox/` (or `~/.local/state/mergefox/`)
   - Windows: `%LOCALAPPDATA%\mergefox\logs\`
4. If the app panicked: `panic.log` in the repo dir or `crash.log` —
   attach both.

If the issue involves a specific repo, the `git log --oneline -20` of
that repo's current branch is usually enough; we rarely need the full
history.

## Security

Please do **not** file security issues as public GitHub issues. See
[`SECURITY.md`](./SECURITY.md) for the private disclosure path.

## License

By contributing you agree your changes are licensed under the same
Apache-2.0 license as the rest of the project (see
[`LICENSE`](./LICENSE) and [`NOTICE`](./NOTICE)).
