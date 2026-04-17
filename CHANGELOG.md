# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Versioning policy (pre-1.0)

While the project is in **`0.y.z`**:

- **`y`** increments may include breaking changes (API, config schema,
  on-disk formats). Each one ships migration notes here.
- **`z`** increments are additive / bug fixes only.
- **`alpha` / `beta` / `rc`** pre-release tags are pre-release and
  may break more freely — all breakage is called out in the
  corresponding release section.

Once we ship `1.0.0`, normal SemVer kicks in: breaking changes bump the
major, additive changes bump the minor, fixes bump the patch. The
on-disk config schema (`config.json`'s `schema` field) versions
independently; migrations are handled in `Config::load`.

## [Unreleased]

## [0.1.0-alpha.2] — 2026-04-17

Second alpha. Ships Sprint 2 (UX basics), Phase 3b (git-control
breadth), and Sprint 4 (MCP autonomy). Binary artifacts are now
proper platform installers (`.dmg` / `.msi` / `.deb` / AppImage) via
`cargo-dist`; builds remain unsigned during the alpha.

### Added
- `.github/workflows/ci.yml` — fmt (advisory) + clippy (advisory) +
  build + test matrix across macOS / Linux / Windows + `cargo-deny` +
  weekly `cargo-audit`.
- `.github/workflows/release.yml` — tag-push release pipeline with
  optional codesign + notarize hooks (macOS + Windows), activated only
  when the matching secrets are configured.
- `deny.toml` — advisories / licenses / bans / sources policy.
- `RELEASE.md` — release process + the list of secrets needed for
  code signing.
- `CONTRIBUTING.md`, `SECURITY.md`, `ARCHITECTURE.md` — first-cut
  contributor docs.
- `src/logging.rs` — `tracing` + `tracing-subscriber` +
  `tracing-appender` with daily rotation, JSON format toggle, and
  OS-appropriate log dirs.
- `src/preflight.rs` — destructive-action pre-flight info. Hard reset,
  delete branch, force push, and drop commit confirmation modals now
  render severity-tagged concrete numbers ("3 commits on `main` will
  be dropped", "2 commits on `origin/main` will be OVERWRITTEN").
- `⌘/Ctrl+Shift+R` — global shortcut to open the reflog recovery
  window. Matches the toolbar button's "recover" mnemonic.
- `MERGEFOX_GIT_TIMEOUT_SECS` — override the 300 s default timeout for
  background git jobs (fetch / push / pull).
- `.git/index.lock` + `HEAD.lock` pre-flight detection before push and
  pull, with actionable messages for fresh (another process running)
  vs stale (crashed process) locks.
- `Repo::rename_remote` + Settings → Repository "Rename to" inline
  row. Default-remote setting auto-migrates if it pointed at the old
  name.
- `Repo::list_worktrees` / `remove_worktree` / `lock_worktree` /
  `unlock_worktree` with a `--porcelain` parser (unit-tested) and a
  Settings → Repository worktree section (main / locked / prunable
  badges, remove + force-remove).
- `CommitAction::CherryPick(Vec<Oid>)` — dispatcher can now run a
  multi-commit cherry-pick sequence, reporting partial success if one
  commit mid-sequence conflicts. UI still passes a single-element vec
  until a multi-select UI lands.
- `Cargo.toml` metadata: `repository`, `homepage`, `documentation`,
  `keywords`, `categories`, `rust-version = "1.76"`.

### Changed
- `println!` / `eprintln!` in non-test code replaced with `tracing`
  events (`warn!` for failures, `debug!` for profiling, `trace!` for
  per-frame instrumentation). `MERGEFOX_LOG_GIT=1` is still honored
  for muscle-memory; new code should prefer
  `MERGEFOX_LOG=mergefox::git::cli=debug`.
- `TODO.md` split and moved to `TODO/` directory:
  - `TODO.md` → `TODO/features.md` (feature gaps)
  - New: `TODO/production.md` (production-readiness roadmap)

### Fixed
- Commit context menu was offering "Delete `refs/stash`" entries —
  `src/git/graph.rs::collect_refs` was classifying every ref that
  wasn't under `refs/remotes/` or `refs/tags/` as a local branch,
  which swept in `refs/stash`, `refs/notes/*`, our own
  `refs/mergefox/autostash-*`, etc. Non-branch refs now skipped at the
  label-collection stage.

### Security
- CI now runs `cargo-deny` on every push and `cargo-audit` weekly so
  new advisories fail the build instead of silently ageing in the
  lockfile.

## [0.1.0-alpha.1] — 2026-04-03

First public alpha. See [`RELEASE_NOTES.md`](./RELEASE_NOTES.md) for
the full list of shipped features and known limitations. Summary:

- Native Rust Git GUI (egui + eframe, glow default, wgpu opt-in).
- git2-free — `gix` on the read path, system `git` on the write path.
- Undo / redo journal with auto-stash + panic recovery modal.
- Multi-tab workspaces.
- Commit workflow, interactive rebase planner, conflict resolver,
  stash management, context-menu history ops.
- Forge integration for GitHub / GitLab / Bitbucket / Gitea /
  Codeberg (PAT + OAuth device flow).
- Pastel commit graph with locally-computed author identicons.

[Unreleased]: https://github.com/JeoungMyeoungHo/MergeFox/compare/v0.1.0-alpha.2...HEAD
[0.1.0-alpha.2]: https://github.com/JeoungMyeoungHo/MergeFox/compare/v0.1.0-alpha.1...v0.1.0-alpha.2
[0.1.0-alpha.1]: https://github.com/JeoungMyeoungHo/MergeFox/releases/tag/v0.1.0-alpha.1
