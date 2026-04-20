//! Project-tree center view — an egui-rendered file-system browser
//! rooted at the repository working directory, with git-status glyphs
//! overlaid on each file row.
//!
//! # Why not reuse the sidebar?
//!
//! The sidebar's working-tree list is a *flat* list of **changed** files
//! — ideal for "everything I'm about to commit" but useless if you want
//! to find the texture you haven't touched yet in a 20k-file game repo.
//! This view inverts that: it shows every on-disk file and overlays the
//! change status, so navigation is by folder / filename first and the
//! git state is secondary decoration.
//!
//! # Lazy walking
//!
//! A naive `walkdir` of a large asset repo is tens-of-thousands of
//! `stat()` calls on every refresh, and (worse) every expansion would
//! rebuild that whole DOM every frame. Instead:
//!
//! - [`ProjectTreeState::build`] walks **only** the repository root.
//! - Subdirectories arrive with `children: None`, meaning "not loaded".
//! - The first time the user expands a directory we `read_dir` that one
//!   level and memoise the result on the node.
//! - Refreshing clears all loaded children back to `None`, so a repo
//!   with 100k files doesn't pay the walk cost unless the user actually
//!   drills in.
//!
//! # Status overlay
//!
//! `git status --porcelain` gives us a flat `Vec<StatusEntry>`. We walk
//! it once per refresh and attach each entry to the matching node,
//! auto-creating the ancestor chain if it wasn't loaded yet (a modified
//! file 6 levels deep must show up even though the user hasn't
//! expanded every intermediate directory). The auto-created ancestors
//! come back as `children: Some(...)` so their own siblings become
//! visible when the user expands them (we don't want the filter to
//! make the parent "look fully loaded" when it wasn't).
//!
//! # Ignored files
//!
//! We call `git ls-files --others --ignored --exclude-standard` once per
//! refresh and grey out matching entries. We deliberately **don't** hide
//! them — users sometimes want to inspect their build output or nested
//! vendor trees, and a silently-hidden file is worse than a visibly
//! dimmed one.
//!
//! # Rendering
//!
//! `ScrollArea::vertical` with an explicit fixed row height. The tree
//! is rendered as a flat list of rows (indentation is just leading
//! space) so virtualisation trivially Just Works — only the visible
//! rows are laid out. A collapsed directory contributes one row, an
//! expanded one contributes 1 + its children's rendered-row count.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use egui::{Color32, RichText};

use crate::git::ops::{EntryKind, StatusEntry};

/// Fixed per-row pixel height. Keeping rows a uniform size lets
/// [`egui::ScrollArea::show_rows`] do the virtualisation work for us —
/// we never lay out rows that are scrolled off-screen, which is the
/// only thing that keeps 50k-entry directories interactive.
const ROW_HEIGHT: f32 = 18.0;

/// Per-level horizontal indent applied to nested rows. Small enough that
/// deep trees still fit inside reasonable sidebars, large enough that
/// the "am I in a child folder?" cue reads at a glance.
const INDENT_PX: f32 = 14.0;

/// A node in the lazily-populated project tree.
///
/// Semantics of `children`:
///
/// - `None` — we haven't tried to read this directory yet. Rendered
///   with a right-pointing chevron the user can click to expand.
/// - `Some(vec)` — we've read it. An empty vec is a real empty folder,
///   distinct from "not loaded yet".
///
/// For files, `children` is always `None` and `kind == File`.
#[derive(Debug, Clone)]
pub struct ProjectTreeNode {
    pub name: String,
    pub rel_path: PathBuf,
    pub kind: ProjectNodeKind,
    pub children: Option<Vec<ProjectTreeNode>>,
    /// Cached git status for this file. `None` for directories and for
    /// files that haven't been touched. Populated by `reapply_status`.
    pub status: Option<FileStatusSummary>,
    /// True when this subtree contains *any* non-clean entry, including
    /// the node itself. Drives the small `●` badge on collapsed folders
    /// so the user can see "there are changes in here" without having
    /// to expand every level.
    pub subtree_dirty: bool,
    /// Present when the repository's lock list (e.g. LFS locks) has an
    /// entry for this file. Rendered as a lock glyph + tooltip with the
    /// owner's name. Directories never carry a lock owner.
    pub lock_owner: Option<String>,
    /// True when this file matches `git ls-files --ignored`. Rendered
    /// dimmer so ignored-but-present files are visible but visually
    /// de-emphasised — we never *hide* ignored files, because the user
    /// often wants to see their own build output.
    pub ignored: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectNodeKind {
    Directory,
    File,
}

/// Per-file status summary attached to a tree node.
///
/// This mirrors a subset of `crate::git::ops::StatusEntry`. We copy the
/// values rather than hold a reference so rendering is `&ProjectTreeNode`
/// with no extra lifetime; the tree is rebuilt from the source `Vec`
/// once per refresh anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStatusSummary {
    pub kind: EntryKind,
    pub staged: bool,
    pub unstaged: bool,
    pub conflicted: bool,
}

impl FileStatusSummary {
    fn from_entry(e: &StatusEntry) -> Self {
        Self {
            kind: e.kind,
            staged: e.staged,
            unstaged: e.unstaged,
            conflicted: e.conflicted,
        }
    }

    /// Any status beyond "clean" — drives the subtree-dirty badge.
    fn is_dirty(self) -> bool {
        self.staged
            || self.unstaged
            || self.conflicted
            || matches!(self.kind, EntryKind::Untracked)
    }
}

pub struct ProjectTreeState {
    pub root: ProjectTreeNode,
    pub expanded: BTreeSet<PathBuf>,
    pub filter: String,
    pub show_hidden: bool,
    /// Last successful rebuild. `None` until the first `build` — in
    /// practice `build` always sets this so it's only ever `None` in
    /// test code that constructs a state manually.
    pub last_refreshed: Option<Instant>,
}

impl ProjectTreeState {
    /// Build an initial tree rooted at `repo_path`. Only the first
    /// directory level is walked; expansions populate themselves on
    /// demand via [`ensure_loaded`]. This keeps opening a repo cheap
    /// even when its working tree has 100k+ entries.
    pub fn build(repo_path: &Path) -> Self {
        let mut root = ProjectTreeNode {
            name: display_root_name(repo_path),
            rel_path: PathBuf::new(),
            kind: ProjectNodeKind::Directory,
            children: None,
            status: None,
            subtree_dirty: false,
            lock_owner: None,
            ignored: false,
        };
        load_directory(&mut root, repo_path, /* show_hidden_placeholder */ false);
        // The root is always "expanded" in the rendering sense — it
        // never shows a chevron. We still walk it eagerly so the first
        // frame isn't an empty tree.
        let mut expanded = BTreeSet::new();
        expanded.insert(PathBuf::new());
        Self {
            root,
            expanded,
            filter: String::new(),
            show_hidden: false,
            last_refreshed: Some(Instant::now()),
        }
    }

    /// Drop all cached children (except the root's first level) and
    /// reload the root. Called from the "Refresh tree" toolbar button
    /// and after any operation that may have touched the working tree
    /// (checkout, reset, stash pop, external editor save, …).
    pub fn refresh_from_disk(&mut self, repo_path: &Path) {
        self.root.children = None;
        load_directory(&mut self.root, repo_path, self.show_hidden);
        // Drop expanded paths that no longer correspond to a directory
        // (a folder may have been deleted between refreshes). Keeping
        // them in the set would be harmless but it saves a scan per
        // paint, and the user wouldn't see them anyway.
        self.expanded.retain(|p| p.as_os_str().is_empty() || repo_path.join(p).is_dir());
        self.last_refreshed = Some(Instant::now());
    }

    /// Overlay fresh `git status` + lock state onto the cached tree.
    ///
    /// Cleared nodes (status becomes `None`) aren't re-walked — we just
    /// zero out the status field on every node first, then re-apply the
    /// current entries. Ancestor-chain creation is done for status
    /// entries whose path lives in a directory we haven't expanded yet,
    /// so the "●" dirty badge appears on the outermost collapsed folder
    /// rather than being invisible.
    pub fn reapply_status(
        &mut self,
        entries: &[StatusEntry],
        ignored_paths: &[PathBuf],
        locks: &[(PathBuf, String)],
    ) {
        clear_status_recursive(&mut self.root);
        for entry in entries {
            attach_status(&mut self.root, &entry.path, FileStatusSummary::from_entry(entry));
        }
        // Ignored files: tag already-loaded nodes. Don't auto-create
        // branches for ignored entries — there can be tens of thousands
        // of them in a `target/` directory and they'd blow up the tree
        // for no benefit.
        for path in ignored_paths {
            tag_ignored(&mut self.root, path);
        }
        // Locks: like status, may live under a path we haven't walked.
        for (path, owner) in locks {
            attach_lock(&mut self.root, path, owner.clone());
        }
        // Finally, bubble `subtree_dirty` up from each leaf so every
        // collapsed parent knows whether its descendants carry changes.
        recompute_subtree_dirty(&mut self.root);
    }

    /// Expand every ancestor directory of `rel_path`. Used by the filter
    /// logic so matches deep in the tree become visible without requiring
    /// the user to click through each intermediate folder.
    pub fn expand_ancestors(&mut self, rel_path: &Path) {
        let mut cur = PathBuf::new();
        self.expanded.insert(cur.clone());
        for comp in rel_path.parent().into_iter().flat_map(|p| p.components()) {
            cur.push(comp);
            self.expanded.insert(cur.clone());
        }
    }
}

/// Best-effort display name for the working-tree root. If the path has
/// no file-name component (root `/`, or a malformed path) we fall back
/// to the full string so the row is never empty.
fn display_root_name(repo_path: &Path) -> String {
    repo_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo_path.display().to_string())
}

/// Read `dir`'s immediate entries (sorted, directories-first) and
/// replace `node.children`. Errors collapse to "empty directory" — a
/// permissions failure is rare enough that the extra UI surface to
/// report it per-row isn't worth the clutter, and the row itself stays
/// visible with its folder name.
fn load_directory(node: &mut ProjectTreeNode, repo_root: &Path, show_hidden: bool) {
    let abs = repo_root.join(&node.rel_path);
    let Ok(rd) = std::fs::read_dir(&abs) else {
        node.children = Some(Vec::new());
        return;
    };
    let mut entries: Vec<ProjectTreeNode> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // `.git` and `.mergefox` are never useful to navigate — hiding
        // them outright even when `show_hidden` is on keeps the mental
        // model simple ("I toggled hidden, why is .git still gone?"
        // has an answer in the README, but most users won't read it;
        // losing `.git` is also a safety win since it's all refs /
        // packfiles they shouldn't open from here anyway).
        if name == ".git" || name == ".mergefox" {
            continue;
        }
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let kind = if ft.is_dir() {
            ProjectNodeKind::Directory
        } else {
            ProjectNodeKind::File
        };
        let rel_path = node.rel_path.join(&name);
        entries.push(ProjectTreeNode {
            name,
            rel_path,
            kind,
            // Directories stay lazy. Files have no children ever but we
            // still store `None` for a uniform shape.
            children: None,
            status: None,
            subtree_dirty: false,
            lock_owner: None,
            ignored: false,
        });
    }
    // Stable, deterministic ordering so tests and snapshot tooling have
    // something reproducible: directories first, then files, each group
    // sorted by name (case-insensitive to avoid "Zoo" coming before
    // "apple" purely because of ASCII order).
    entries.sort_by(|a, b| match (a.kind, b.kind) {
        (ProjectNodeKind::Directory, ProjectNodeKind::File) => std::cmp::Ordering::Less,
        (ProjectNodeKind::File, ProjectNodeKind::Directory) => std::cmp::Ordering::Greater,
        _ => a
            .name
            .to_lowercase()
            .cmp(&b.name.to_lowercase()),
    });
    node.children = Some(entries);
}

/// Recursively null out every node's status / lock / ignored flag so a
/// fresh overlay starts from a known-clean state. We deliberately keep
/// the tree structure itself intact — rebuilding it would throw away
/// the user's current expansion set.
fn clear_status_recursive(node: &mut ProjectTreeNode) {
    node.status = None;
    node.subtree_dirty = false;
    node.lock_owner = None;
    node.ignored = false;
    if let Some(children) = &mut node.children {
        for c in children {
            clear_status_recursive(c);
        }
    }
}

/// Walk down from `node` along `rel_path`, creating any missing branch
/// nodes as `children: Some(...)` as we go, and attach `status` to the
/// leaf. This is how a modified file living under an as-yet-unexpanded
/// directory still surfaces its parent's dirty badge — we synthesize
/// just enough of the tree to carry the status upward.
fn attach_status(node: &mut ProjectTreeNode, rel_path: &Path, status: FileStatusSummary) {
    let components: Vec<_> = rel_path.components().collect();
    attach_status_inner(node, &components, 0, status);
}

fn attach_status_inner(
    node: &mut ProjectTreeNode,
    components: &[std::path::Component<'_>],
    depth: usize,
    status: FileStatusSummary,
) {
    if depth == components.len() {
        // Reached the leaf — attach the status here and return.
        node.status = Some(status);
        return;
    }
    let comp = components[depth];
    let name = comp.as_os_str().to_string_lossy().into_owned();
    let children = node.children.get_or_insert_with(Vec::new);
    // Find-or-create the child.
    let idx = match children.iter().position(|c| c.name == name) {
        Some(i) => i,
        None => {
            let rel_path: PathBuf = components[..=depth]
                .iter()
                .map(|c| c.as_os_str())
                .collect();
            // Synthetic-branch nodes are marked as directories for all
            // but the final component — the leaf is a file.
            let kind = if depth + 1 == components.len() {
                ProjectNodeKind::File
            } else {
                ProjectNodeKind::Directory
            };
            children.push(ProjectTreeNode {
                name,
                rel_path,
                kind,
                children: if kind == ProjectNodeKind::Directory {
                    // Mark as loaded (empty) so the user gets an
                    // expand affordance that doesn't promise more
                    // siblings than actually exist under the synthetic
                    // branch. If the real directory has more siblings
                    // they'll appear after the user clicks "Refresh".
                    Some(Vec::new())
                } else {
                    None
                },
                status: None,
                subtree_dirty: false,
                lock_owner: None,
                ignored: false,
            });
            children.len() - 1
        }
    };
    attach_status_inner(&mut children[idx], components, depth + 1, status);
}

/// Walk existing nodes along `rel_path`; no-op if any segment hasn't
/// been loaded yet. Unlike `attach_status`, we specifically DO NOT
/// auto-create branches here — ignored trees can be enormous and
/// materialising them would defeat the lazy-walk design.
fn tag_ignored(node: &mut ProjectTreeNode, rel_path: &Path) {
    let components: Vec<_> = rel_path.components().collect();
    let mut cur = node;
    for comp in &components {
        let name = comp.as_os_str().to_string_lossy();
        let Some(children) = &mut cur.children else {
            return;
        };
        let Some(idx) = children.iter().position(|c| c.name == *name) else {
            return;
        };
        cur = &mut children[idx];
    }
    cur.ignored = true;
}

fn attach_lock(node: &mut ProjectTreeNode, rel_path: &Path, owner: String) {
    let components: Vec<_> = rel_path.components().collect();
    attach_lock_inner(node, &components, 0, owner);
}

fn attach_lock_inner(
    node: &mut ProjectTreeNode,
    components: &[std::path::Component<'_>],
    depth: usize,
    owner: String,
) {
    if depth == components.len() {
        node.lock_owner = Some(owner);
        return;
    }
    let name = components[depth].as_os_str().to_string_lossy().into_owned();
    let Some(children) = &mut node.children else {
        return; // Not loaded — we don't synthesize branches for locks.
    };
    let Some(idx) = children.iter().position(|c| c.name == name) else {
        return;
    };
    attach_lock_inner(&mut children[idx], components, depth + 1, owner);
}

/// Bubble `subtree_dirty` upward in a single post-order traversal.
/// Returns the computed value for the node so the caller can OR it
/// into its own accumulator.
fn recompute_subtree_dirty(node: &mut ProjectTreeNode) -> bool {
    let mut any = node.status.map(|s| s.is_dirty()).unwrap_or(false);
    if let Some(children) = &mut node.children {
        for c in children {
            any |= recompute_subtree_dirty(c);
        }
    }
    node.subtree_dirty = any;
    any
}

// ---------- rendering ----------

pub struct ShowOptions<'a> {
    pub rules: &'a crate::ui::profile_rules::ProfileRules,
    pub selected_file: Option<&'a Path>,
    pub available_width: f32,
}

/// Actions the user can trigger from the tree. Rendering is side-effect
/// free — we return an intent and the caller (usually `main_panel`)
/// applies it against the real app state. This mirrors the pattern the
/// graph view uses and lets us unit-test rendering without a live repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectTreeIntent {
    SelectFile(PathBuf),
    ToggleExpand(PathBuf),
    OpenInFileManager(PathBuf),
    StageFile(PathBuf),
    UnstageFile(PathBuf),
    RequestLock(PathBuf),
    RequestUnlock(PathBuf),
    RequestForceUnlock(PathBuf),
    DiscardFile(PathBuf),
}

/// Render the tree. Returns at most one intent per frame — the caller
/// applies it after the UI closure releases its borrow, same as every
/// other dispatch path in the app.
pub fn show(
    ui: &mut egui::Ui,
    state: &mut ProjectTreeState,
    opts: ShowOptions,
) -> Option<ProjectTreeIntent> {
    let mut intent: Option<ProjectTreeIntent> = None;
    let _ = opts.available_width; // room for future column-width-aware layout

    // If a filter is active, make sure every ancestor of a matching file
    // is expanded *this frame*. We do this up front so the render pass
    // sees a consistent `expanded` set.
    let filter_lc = state.filter.trim().to_ascii_lowercase();
    if !filter_lc.is_empty() {
        let mut to_expand: Vec<PathBuf> = Vec::new();
        collect_filter_ancestors(&state.root, &filter_lc, &mut to_expand);
        for p in to_expand {
            state.expanded.insert(p);
        }
    }

    // Pre-flatten the tree into a render list. The list is small enough
    // (only expanded paths contribute rows) that rebuilding it per paint
    // is cheap, and it lets us pass the row list to `show_rows` for
    // virtualisation.
    let rows = flatten_for_render(&state.root, &state.expanded, &filter_lc);
    let selected = opts.selected_file;

    egui::ScrollArea::vertical()
        .id_salt("project_tree_scroll")
        .auto_shrink([false, false])
        .show_rows(ui, ROW_HEIGHT, rows.len(), |ui, row_range| {
            for i in row_range {
                let row = &rows[i];
                if let Some(pressed) = render_row(ui, row, selected, opts.rules) {
                    // Only keep the first intent produced — later rows in
                    // the same frame are ignored, which is fine because
                    // clicking two rows in a single frame isn't possible
                    // with a normal input device.
                    intent.get_or_insert(pressed);
                }
            }
        });

    intent
}

/// Flat representation of a single visible row — computed per frame.
/// Borrows the node instead of cloning so rebuilding the list stays
/// cheap even with thousands of visible rows.
struct RenderRow<'a> {
    node: &'a ProjectTreeNode,
    depth: usize,
    expanded: bool,
}

fn flatten_for_render<'a>(
    root: &'a ProjectTreeNode,
    expanded: &BTreeSet<PathBuf>,
    filter_lc: &str,
) -> Vec<RenderRow<'a>> {
    let mut out = Vec::new();
    // Skip the root node itself — the tree view shows its children as
    // the top-level rows. A repo's own folder name is already in the
    // tab strip, repeating it here would waste vertical space.
    if let Some(children) = &root.children {
        for c in children {
            walk(c, 0, expanded, filter_lc, &mut out);
        }
    }
    out
}

fn walk<'a>(
    node: &'a ProjectTreeNode,
    depth: usize,
    expanded: &BTreeSet<PathBuf>,
    filter_lc: &str,
    out: &mut Vec<RenderRow<'a>>,
) {
    // Filter logic: a row is visible iff either
    //   (a) the filter is empty, OR
    //   (b) the node's name matches the filter, OR
    //   (c) any descendant matches (so the user sees the ancestor chain
    //       leading to a match).
    if !filter_lc.is_empty()
        && !node.name.to_ascii_lowercase().contains(filter_lc)
        && !subtree_matches_filter(node, filter_lc)
    {
        return;
    }
    let is_expanded = matches!(node.kind, ProjectNodeKind::Directory)
        && expanded.contains(&node.rel_path);
    out.push(RenderRow {
        node,
        depth,
        expanded: is_expanded,
    });
    if is_expanded {
        if let Some(children) = &node.children {
            for c in children {
                walk(c, depth + 1, expanded, filter_lc, out);
            }
        }
    }
}

fn subtree_matches_filter(node: &ProjectTreeNode, filter_lc: &str) -> bool {
    if node.name.to_ascii_lowercase().contains(filter_lc) {
        return true;
    }
    if let Some(children) = &node.children {
        for c in children {
            if subtree_matches_filter(c, filter_lc) {
                return true;
            }
        }
    }
    false
}

/// Recursively collect ancestor paths of any node whose name matches
/// the active filter. Used to auto-expand those branches so matches
/// nested under collapsed directories become reachable.
fn collect_filter_ancestors(node: &ProjectTreeNode, filter_lc: &str, out: &mut Vec<PathBuf>) {
    let Some(children) = &node.children else {
        return;
    };
    for c in children {
        if c.name.to_ascii_lowercase().contains(filter_lc) {
            // Push every ancestor up to (but not including) this node.
            let mut cur = c.rel_path.clone();
            while cur.pop() {
                out.push(cur.clone());
            }
            out.push(PathBuf::new()); // root
        }
        collect_filter_ancestors(c, filter_lc, out);
    }
}

fn render_row(
    ui: &mut egui::Ui,
    row: &RenderRow<'_>,
    selected: Option<&Path>,
    rules: &crate::ui::profile_rules::ProfileRules,
) -> Option<ProjectTreeIntent> {
    let mut intent = None;
    let indent = row.depth as f32 * INDENT_PX;
    let is_selected = matches!(row.node.kind, ProjectNodeKind::File)
        && selected.map_or(false, |p| p == row.node.rel_path);

    let row_resp = ui
        .horizontal(|ui| {
            ui.add_space(indent);

            // Directory chevron / file spacer — always the same width
            // so the name column stays aligned across rows.
            let chevron = match row.node.kind {
                ProjectNodeKind::Directory => {
                    if row.expanded {
                        "▼ "
                    } else {
                        "▶ "
                    }
                }
                ProjectNodeKind::File => "   ",
            };
            ui.monospace(chevron);

            // Status glyph, if any. Directories show the dirty dot when
            // their subtree has changes; files show their per-file
            // letter ("M" / "A" / "D" / "?" / "!").
            match row.node.kind {
                ProjectNodeKind::Directory => {
                    if row.node.subtree_dirty {
                        ui.colored_label(
                            Color32::from_rgb(220, 190, 90),
                            "●",
                        );
                    } else {
                        ui.monospace(" ");
                    }
                }
                ProjectNodeKind::File => {
                    if let Some(status) = row.node.status {
                        let (color, glyph) = status_style(status);
                        ui.colored_label(color, glyph);
                    } else {
                        ui.monospace(" ");
                    }
                }
            }

            // Name. Dimmed if `ignored`, highlighted if selected.
            let base = RichText::new(&row.node.name).monospace();
            let text = if row.node.ignored {
                base.color(Color32::from_gray(110))
            } else if is_selected {
                base.strong()
            } else {
                base
            };
            let label = ui.add(egui::Label::new(text).sense(egui::Sense::click()));

            // Lock badge — only rendered when profile enables lock
            // controls AND the file actually carries a lock. Tooltip
            // has the owner name so the user can tell at a glance who
            // grabbed it.
            if rules.show_lfs_lock_controls {
                if let Some(owner) = &row.node.lock_owner {
                    ui.label(
                        RichText::new("🔒")
                            .color(Color32::from_rgb(200, 160, 100))
                            .small(),
                    )
                    .on_hover_text(format!("Locked by {owner}"));
                }
            }

            if label.clicked() {
                intent = Some(match row.node.kind {
                    ProjectNodeKind::Directory => {
                        ProjectTreeIntent::ToggleExpand(row.node.rel_path.clone())
                    }
                    ProjectNodeKind::File => {
                        ProjectTreeIntent::SelectFile(row.node.rel_path.clone())
                    }
                });
            }

            // Context menu. Only files get the file-level actions;
            // directories currently have no useful right-click ops
            // beyond "expand" which is already the left-click.
            if matches!(row.node.kind, ProjectNodeKind::File) {
                label.context_menu(|ui| {
                    if ui.button("Open").clicked() {
                        intent = Some(ProjectTreeIntent::SelectFile(row.node.rel_path.clone()));
                        ui.close_menu();
                    }
                    if ui.button("Show in file manager").clicked() {
                        intent = Some(ProjectTreeIntent::OpenInFileManager(
                            row.node.rel_path.clone(),
                        ));
                        ui.close_menu();
                    }
                    ui.separator();
                    // Staging is only meaningful for files with some
                    // kind of change. We show both items when a file is
                    // partially staged; grey both out when it's clean.
                    let status = row.node.status;
                    let has_staged = status.map_or(false, |s| s.staged);
                    let has_unstaged_or_untracked = status.map_or(false, |s| {
                        s.unstaged || matches!(s.kind, EntryKind::Untracked)
                    });
                    ui.add_enabled_ui(has_unstaged_or_untracked, |ui| {
                        if ui.button("Stage file").clicked() {
                            intent = Some(ProjectTreeIntent::StageFile(
                                row.node.rel_path.clone(),
                            ));
                            ui.close_menu();
                        }
                    });
                    ui.add_enabled_ui(has_staged, |ui| {
                        if ui.button("Unstage file").clicked() {
                            intent = Some(ProjectTreeIntent::UnstageFile(
                                row.node.rel_path.clone(),
                            ));
                            ui.close_menu();
                        }
                    });
                    ui.add_enabled_ui(has_unstaged_or_untracked, |ui| {
                        if ui.button("Discard changes").clicked() {
                            intent = Some(ProjectTreeIntent::DiscardFile(
                                row.node.rel_path.clone(),
                            ));
                            ui.close_menu();
                        }
                    });
                    if rules.show_lfs_lock_controls {
                        ui.separator();
                        let locked = row.node.lock_owner.is_some();
                        if !locked && ui.button("Lock").clicked() {
                            intent = Some(ProjectTreeIntent::RequestLock(
                                row.node.rel_path.clone(),
                            ));
                            ui.close_menu();
                        }
                        if locked && ui.button("Unlock").clicked() {
                            intent = Some(ProjectTreeIntent::RequestUnlock(
                                row.node.rel_path.clone(),
                            ));
                            ui.close_menu();
                        }
                        if locked && ui.button("Force unlock").clicked() {
                            intent = Some(ProjectTreeIntent::RequestForceUnlock(
                                row.node.rel_path.clone(),
                            ));
                            ui.close_menu();
                        }
                    }
                });
            }
        })
        .response;

    // A click anywhere on the row (not just on the label) toggles /
    // selects. We catch this as a fallback for clicks on the chevron /
    // status glyph columns; `clicked` on the label already covered the
    // main name hit-box above.
    if row_resp.clicked() && intent.is_none() {
        intent = Some(match row.node.kind {
            ProjectNodeKind::Directory => {
                ProjectTreeIntent::ToggleExpand(row.node.rel_path.clone())
            }
            ProjectNodeKind::File => ProjectTreeIntent::SelectFile(row.node.rel_path.clone()),
        });
    }

    intent
}

/// Colour + glyph for a file-status letter. Matches the commit-modal
/// palette so the two views feel like the same app rather than two
/// separate implementations drifting apart over time.
fn status_style(status: FileStatusSummary) -> (Color32, &'static str) {
    if status.conflicted {
        return (Color32::from_rgb(255, 80, 80), "!");
    }
    match status.kind {
        EntryKind::Untracked => (Color32::from_rgb(90, 180, 120), "?"),
        EntryKind::New => (Color32::from_rgb(90, 180, 120), "A"),
        EntryKind::Modified => (Color32::from_rgb(220, 190, 90), "M"),
        EntryKind::Deleted => (Color32::from_rgb(220, 100, 100), "D"),
        EntryKind::Renamed => (Color32::from_rgb(150, 150, 220), "R"),
        EntryKind::Typechange => (Color32::from_rgb(200, 120, 200), "T"),
        EntryKind::Conflicted => (Color32::from_rgb(255, 80, 80), "!"),
    }
}

// -------------------------- tests --------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_status(path: &str, kind: EntryKind) -> StatusEntry {
        StatusEntry {
            path: PathBuf::from(path),
            kind,
            staged: matches!(kind, EntryKind::New),
            unstaged: matches!(kind, EntryKind::Modified),
            conflicted: matches!(kind, EntryKind::Conflicted),
        }
    }

    /// Materialise a tempdir with a small, known layout:
    /// ```
    /// root/
    ///   aa/
    ///     inner.txt
    ///   zz.txt
    ///   .hidden
    /// ```
    fn scratch_tree() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "mergefox-project-tree-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("aa")).unwrap();
        std::fs::write(root.join("aa/inner.txt"), b"x").unwrap();
        std::fs::write(root.join("zz.txt"), b"y").unwrap();
        std::fs::write(root.join(".hidden"), b"z").unwrap();
        root
    }

    #[test]
    fn build_walks_only_top_level_and_sorts_dirs_first() {
        let root = scratch_tree();
        let state = ProjectTreeState::build(&root);
        let children = state.root.children.as_ref().expect("root loaded");
        // Dotfile is hidden by default; directories first → "aa",
        // then file "zz.txt".
        let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["aa", "zz.txt"]);
        // "aa" must NOT be pre-walked — its children should be None so
        // the user's first expand triggers the read_dir.
        let aa = children.iter().find(|c| c.name == "aa").unwrap();
        assert!(
            aa.children.is_none(),
            "directories must stay lazy after initial build"
        );
    }

    #[test]
    fn build_respects_show_hidden_toggle_on_refresh() {
        let root = scratch_tree();
        let mut state = ProjectTreeState::build(&root);
        // Default: hidden entries skipped.
        let names: Vec<&str> = state
            .root
            .children
            .as_ref()
            .unwrap()
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(!names.contains(&".hidden"));
        // Toggle on and refresh — `.hidden` should now appear, but
        // `.git` (if it existed) would still be filtered.
        state.show_hidden = true;
        state.refresh_from_disk(&root);
        let names_after: Vec<&str> = state
            .root
            .children
            .as_ref()
            .unwrap()
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(names_after.contains(&".hidden"));
    }

    #[test]
    fn reapply_status_propagates_subtree_dirty_to_ancestors() {
        // Synthetic tree — we don't need a real directory for the
        // status-attach / subtree-dirty logic.
        let mut state = ProjectTreeState {
            root: ProjectTreeNode {
                name: "root".into(),
                rel_path: PathBuf::new(),
                kind: ProjectNodeKind::Directory,
                children: Some(vec![ProjectTreeNode {
                    name: "foo".into(),
                    rel_path: PathBuf::from("foo"),
                    kind: ProjectNodeKind::Directory,
                    children: Some(vec![ProjectTreeNode {
                        name: "bar".into(),
                        rel_path: PathBuf::from("foo/bar"),
                        kind: ProjectNodeKind::Directory,
                        children: Some(vec![ProjectTreeNode {
                            name: "leaf.txt".into(),
                            rel_path: PathBuf::from("foo/bar/leaf.txt"),
                            kind: ProjectNodeKind::File,
                            children: None,
                            status: None,
                            subtree_dirty: false,
                            lock_owner: None,
                            ignored: false,
                        }]),
                        status: None,
                        subtree_dirty: false,
                        lock_owner: None,
                        ignored: false,
                    }]),
                    status: None,
                    subtree_dirty: false,
                    lock_owner: None,
                    ignored: false,
                }]),
                status: None,
                subtree_dirty: false,
                lock_owner: None,
                ignored: false,
            },
            expanded: BTreeSet::new(),
            filter: String::new(),
            show_hidden: false,
            last_refreshed: None,
        };

        state.reapply_status(
            &[mk_status("foo/bar/leaf.txt", EntryKind::Modified)],
            &[],
            &[],
        );

        // The leaf itself is dirty, and so are both ancestors.
        let foo = &state.root.children.as_ref().unwrap()[0];
        assert!(foo.subtree_dirty, "foo must light up");
        let bar = &foo.children.as_ref().unwrap()[0];
        assert!(bar.subtree_dirty, "foo/bar must light up");
        let leaf = &bar.children.as_ref().unwrap()[0];
        assert!(
            leaf.status.map_or(false, |s| matches!(s.kind, EntryKind::Modified)),
            "leaf must carry the Modified status",
        );
    }

    #[test]
    fn reapply_status_synthesizes_branch_for_unloaded_path() {
        // Start with a tree whose "foo" subtree hasn't been walked yet.
        let mut state = ProjectTreeState {
            root: ProjectTreeNode {
                name: "root".into(),
                rel_path: PathBuf::new(),
                kind: ProjectNodeKind::Directory,
                children: Some(vec![ProjectTreeNode {
                    name: "foo".into(),
                    rel_path: PathBuf::from("foo"),
                    kind: ProjectNodeKind::Directory,
                    children: None, // unloaded!
                    status: None,
                    subtree_dirty: false,
                    lock_owner: None,
                    ignored: false,
                }]),
                status: None,
                subtree_dirty: false,
                lock_owner: None,
                ignored: false,
            },
            expanded: BTreeSet::new(),
            filter: String::new(),
            show_hidden: false,
            last_refreshed: None,
        };
        state.reapply_status(
            &[mk_status("foo/inner/changed.txt", EntryKind::Modified)],
            &[],
            &[],
        );
        let foo = &state.root.children.as_ref().unwrap()[0];
        // Branch synthesis must have kicked in — `foo` now has children
        // and the dirty flag has bubbled all the way up.
        assert!(foo.subtree_dirty);
        assert!(foo.children.is_some());
    }

    #[test]
    fn filter_expand_ancestors_covers_nested_match() {
        // foo/bar/baz.txt — filtering for "baz" must expand foo + foo/bar.
        let state = ProjectTreeState {
            root: ProjectTreeNode {
                name: "root".into(),
                rel_path: PathBuf::new(),
                kind: ProjectNodeKind::Directory,
                children: Some(vec![ProjectTreeNode {
                    name: "foo".into(),
                    rel_path: PathBuf::from("foo"),
                    kind: ProjectNodeKind::Directory,
                    children: Some(vec![ProjectTreeNode {
                        name: "bar".into(),
                        rel_path: PathBuf::from("foo/bar"),
                        kind: ProjectNodeKind::Directory,
                        children: Some(vec![ProjectTreeNode {
                            name: "baz.txt".into(),
                            rel_path: PathBuf::from("foo/bar/baz.txt"),
                            kind: ProjectNodeKind::File,
                            children: None,
                            status: None,
                            subtree_dirty: false,
                            lock_owner: None,
                            ignored: false,
                        }]),
                        status: None,
                        subtree_dirty: false,
                        lock_owner: None,
                        ignored: false,
                    }]),
                    status: None,
                    subtree_dirty: false,
                    lock_owner: None,
                    ignored: false,
                }]),
                status: None,
                subtree_dirty: false,
                lock_owner: None,
                ignored: false,
            },
            expanded: BTreeSet::new(),
            filter: "baz".into(),
            show_hidden: false,
            last_refreshed: None,
        };
        let mut out = Vec::new();
        collect_filter_ancestors(&state.root, "baz", &mut out);
        assert!(out.contains(&PathBuf::new()));
        assert!(out.contains(&PathBuf::from("foo")));
        assert!(out.contains(&PathBuf::from("foo/bar")));
    }

    #[test]
    fn expand_ancestors_walks_parent_chain() {
        let mut state = ProjectTreeState {
            root: ProjectTreeNode {
                name: "root".into(),
                rel_path: PathBuf::new(),
                kind: ProjectNodeKind::Directory,
                children: Some(Vec::new()),
                status: None,
                subtree_dirty: false,
                lock_owner: None,
                ignored: false,
            },
            expanded: BTreeSet::new(),
            filter: String::new(),
            show_hidden: false,
            last_refreshed: None,
        };
        state.expand_ancestors(Path::new("a/b/c.txt"));
        assert!(state.expanded.contains(&PathBuf::new()));
        assert!(state.expanded.contains(&PathBuf::from("a")));
        assert!(state.expanded.contains(&PathBuf::from("a/b")));
        // The leaf itself is NOT expanded — we only open its ancestors.
        assert!(!state.expanded.contains(&PathBuf::from("a/b/c.txt")));
    }

    #[test]
    fn attach_lock_requires_loaded_path_and_no_synthesis() {
        // Locks land only on nodes we've already walked. An unloaded
        // subtree is *ignored*, not synthesised, because lock lists can
        // be huge and we don't want to materialise them into the tree.
        let mut state = ProjectTreeState {
            root: ProjectTreeNode {
                name: "root".into(),
                rel_path: PathBuf::new(),
                kind: ProjectNodeKind::Directory,
                children: Some(vec![ProjectTreeNode {
                    name: "foo".into(),
                    rel_path: PathBuf::from("foo"),
                    kind: ProjectNodeKind::Directory,
                    children: None,
                    status: None,
                    subtree_dirty: false,
                    lock_owner: None,
                    ignored: false,
                }]),
                status: None,
                subtree_dirty: false,
                lock_owner: None,
                ignored: false,
            },
            expanded: BTreeSet::new(),
            filter: String::new(),
            show_hidden: false,
            last_refreshed: None,
        };
        state.reapply_status(
            &[],
            &[],
            &[(PathBuf::from("foo/locked.bin"), "alice".to_string())],
        );
        let foo = &state.root.children.as_ref().unwrap()[0];
        // `foo` itself stays unloaded — no lock branch was synthesized.
        assert!(foo.children.is_none());
        assert!(foo.lock_owner.is_none());
    }
}
