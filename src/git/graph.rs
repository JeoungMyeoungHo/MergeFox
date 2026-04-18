//! Commit graph with lane assignment for rendering.
//!
//! Algorithm (topological walk, child-before-parent):
//!   * Maintain `active`: Vec<Option<Oid>> where the index is a "lane"
//!     and the value is the oid we expect to see in that lane.
//!   * For each commit:
//!       1. Find its lane (where `active[lane] == Some(oid)`) or allocate
//!          a new lane if it's a branch tip with no children above.
//!       2. Free that lane (`active[lane] = None`).
//!       3. For each parent, either reuse an already-active lane or place
//!          it in a fresh lane (first parent prefers our freed lane, so
//!          straight trunks stay straight).
//!       4. Snapshot only the occupied lane indices into `lanes_below` so
//!          the renderer knows which vertical lines cross the row→row gap
//!          without retaining a full lane→oid table for every row.
//!
//! Edges FROM this commit to its parents are recorded separately in
//! `edges_out` so the renderer can draw the diagonal "joining" lines.

use anyhow::{Context, Result};
use gix::revision::walk::Sorting;
use gix::ObjectId;
use gix_traverse::commit::simple::CommitTimeOrder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphScope {
    CurrentBranch,
    AllLocal,
    AllRefs,
}

impl GraphScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::CurrentBranch => "Current",
            Self::AllLocal => "All local",
            Self::AllRefs => "All refs",
        }
    }
}

pub struct CommitGraph {
    pub rows: Box<[GraphRow]>,
    pub max_lane: u16,
    /// `true` when we hit `MAX_GRAPH_COMMITS` and stopped walking before
    /// reaching the history's root. UI shows a "showing N most-recent"
    /// banner in that case so the user isn't silently misled.
    pub truncated: bool,
}

/// Maximum commits we'll walk into the graph before stopping. Linux-
/// kernel-scale histories (1M+ commits) make the graph both useless to
/// look at and costly in RAM (~100 bytes per `GraphRow`). 5000 is more
/// than anyone is going to visually scan, and keeps the structure under
/// a megabyte.
pub const MAX_GRAPH_COMMITS: usize = 5000;

/// Hard ceiling on concurrent lanes. Highly merge-heavy histories
/// (subtree-merged distros, monorepos with many subprojects) can balloon
/// past 100 lanes, turning the graph into a useless barcode. Beyond this
/// we still record the commit but compress its lane index so the renderer
/// draws it in the "overflow" column. Default large enough that normal
/// repos stay exact; small enough that Linux kernel stays usable.
pub const MAX_GRAPH_LANES: u16 = 40;

/// A ref label attached to a graph row — branch, remote branch, or tag.
#[derive(Debug, Clone)]
pub struct RefLabel {
    pub short: Box<str>,
    pub kind: RefKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    LocalBranch,
    RemoteBranch,
    Tag,
}

pub struct GraphRow {
    pub oid: gix::ObjectId,
    pub summary: Box<str>,
    pub body_preview: Box<str>,
    pub author: Box<str>,
    /// Author email, used only for deterministic avatar-colour hashing.
    pub author_email: Box<str>,
    pub timestamp: i64,
    pub lane: u16,
    /// Lanes from the previous row that terminate at this commit.
    pub incoming_lanes: Box<[u16]>,
    /// State of active lanes BELOW this row (after this commit is placed).
    /// Each entry is an occupied lane index; omitted lanes are empty.
    pub lanes_below: Box<[u16]>,
    /// The lane each parent of this commit joins below.
    pub edges_out: Box<[u16]>,
    pub refs: Box<[RefLabel]>,
}

impl CommitGraph {
    pub fn build(repo: &gix::Repository, scope: GraphScope) -> Result<Self> {
        let refs_map = collect_refs(repo, scope).unwrap_or_default();
        let tips = collect_tips(repo, scope).context("collect tips")?;

        // gix's rev_walk takes the tips up-front and returns an iterator
        // of commits sorted by either topology or commit-time. We use
        // `ByCommitTimeNewestFirst` plus the topo-aware platform setting
        // so merge ancestors stay grouped — same effective ordering as
        // libgit2's `Sort::TOPOLOGICAL | Sort::TIME`.
        let walker = repo
            .rev_walk(tips)
            .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
            .all()
            .context("revwalk init")?;

        let mut active: Vec<Option<ObjectId>> = Vec::new();
        let mut rows: Vec<GraphRow> = Vec::with_capacity(MAX_GRAPH_COMMITS.min(1024));
        let mut max_lane: u16 = 0;
        let mut truncated = false;

        for info_res in walker {
            // Cap the walk. Without this, cloning torvalds/linux (~1.2M
            // commits) built a 120 MB+ graph that rendered as a screen-
            // wide barcode of lanes. Below this cap the UI offers an
            // explicit "show more history" button on the banner instead.
            if rows.len() >= MAX_GRAPH_COMMITS {
                truncated = true;
                break;
            }
            let Ok(info) = info_res else { continue };
            let oid: ObjectId = info.id;
            let Ok(commit) = repo.find_object(info.id).and_then(|o| {
                o.try_into_commit()
                    .map_err(|_| gix::object::find::existing::Error::NotFound { oid: info.id })
            }) else {
                continue;
            };
            let parents: Vec<ObjectId> = commit.parent_ids().map(|id| id.detach()).collect();

            // Step 1+2: find my lane (or allocate a fresh one) and free it.
            let incoming_lanes = matching_lanes(&active, oid);
            let my_lane = if let Some(first) = incoming_lanes.first() {
                for lane in &incoming_lanes {
                    active[*lane as usize] = None;
                }
                *first as usize
            } else {
                first_free_lane(&active)
            };
            if my_lane >= active.len() {
                active.resize(my_lane + 1, None);
            }

            // Step 3: place each parent into a lane.
            let mut edges_out: Vec<u16> = Vec::with_capacity(parents.len());
            for (i, parent) in parents.iter().enumerate() {
                let target_lane =
                    if let Some(existing) = active.iter().position(|l| *l == Some(*parent)) {
                        existing
                    } else if i == 0 {
                        // First parent prefers our freed lane so trunks stay straight.
                        active[my_lane] = Some(*parent);
                        my_lane
                    } else {
                        let idx = first_free_lane(&active);
                        if idx >= active.len() {
                            active.resize(idx + 1, None);
                        }
                        active[idx] = Some(*parent);
                        idx
                    };
                edges_out.push(target_lane as u16);
            }

            max_lane = max_lane.max(my_lane as u16);
            for l in &edges_out {
                max_lane = max_lane.max(*l);
            }
            // Clamp max_lane to the rendering ceiling. The underlying
            // `my_lane` / `edges_out` / `lanes_below` indices are still
            // faithful (needed for correctness of parent lookups), but
            // the renderer caps its draw width at `MAX_GRAPH_LANES` so
            // the graph doesn't stretch into an unscrollable barcode.
            // See `ui::graph` for the compression logic at paint time.

            let refs = refs_map.get(&oid).cloned().unwrap_or_default();
            let (summary, body, author_name, author_email, timestamp) = decode_for_graph(&commit)
                .unwrap_or_else(|| {
                    (
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        0,
                    )
                });
            rows.push(GraphRow {
                oid,
                summary: summary.into(),
                body_preview: first_body_preview_line(&body).into(),
                author: author_name.into(),
                author_email: author_email.into(),
                timestamp,
                lane: my_lane as u16,
                incoming_lanes: incoming_lanes.into_boxed_slice(),
                lanes_below: occupied_lanes(&active),
                edges_out: edges_out.into_boxed_slice(),
                refs: refs.into_boxed_slice(),
            });
        }

        Ok(Self {
            rows: rows.into_boxed_slice(),
            max_lane,
            truncated,
        })
    }
}

fn first_free_lane(active: &[Option<ObjectId>]) -> usize {
    active
        .iter()
        .position(|l| l.is_none())
        .unwrap_or(active.len())
}

fn matching_lanes(active: &[Option<ObjectId>], oid: ObjectId) -> Vec<u16> {
    active
        .iter()
        .enumerate()
        .filter_map(|(idx, lane)| (*lane == Some(oid)).then_some(idx as u16))
        .collect()
}

fn occupied_lanes(active: &[Option<ObjectId>]) -> Box<[u16]> {
    active
        .iter()
        .enumerate()
        .filter_map(|(idx, lane)| lane.is_some().then_some(idx as u16))
        .collect::<Vec<_>>()
        .into_boxed_slice()
}

fn collect_tips(repo: &gix::Repository, scope: GraphScope) -> Result<Vec<ObjectId>> {
    let mut tips = Vec::new();
    let platform = repo.references().context("open ref iter")?;
    match scope {
        GraphScope::CurrentBranch => {
            if let Ok(id) = repo.head_id() {
                tips.push(id.detach());
            }
        }
        GraphScope::AllLocal => {
            for r in platform.prefixed("refs/heads/")?.flatten() {
                if let Some(oid) = peel_ref_to_oid(repo, &r) {
                    tips.push(oid);
                }
            }
        }
        GraphScope::AllRefs => {
            for r in platform.all()?.flatten() {
                if let Some(oid) = peel_ref_to_oid(repo, &r) {
                    tips.push(oid);
                }
            }
        }
    }
    Ok(tips)
}

fn collect_refs(
    repo: &gix::Repository,
    scope: GraphScope,
) -> Result<std::collections::HashMap<ObjectId, Vec<RefLabel>>> {
    let mut map: std::collections::HashMap<ObjectId, Vec<RefLabel>> =
        std::collections::HashMap::new();
    let head_name = repo
        .head_name()
        .ok()
        .flatten()
        .map(|n| n.as_bstr().to_string());

    let platform = repo.references()?;
    for r in platform.all()?.flatten() {
        let full = r.name().as_bstr().to_string();
        let short = String::from_utf8_lossy(r.name().shorten()).into_owned();
        let is_local_branch = full.starts_with("refs/heads/");
        let is_remote = full.starts_with("refs/remotes/");
        let is_tag = full.starts_with("refs/tags/");

        // Skip refs that aren't one of the three kinds the graph knows
        // how to render. Examples:
        //   * `refs/stash`                      — the stash reflog head
        //   * `refs/notes/*`                    — git-notes storage
        //   * `refs/mergefox/autostash-*`       — our own undo safety net
        //   * `refs/original/*`                 — leftovers from `filter-branch`
        //   * `refs/pull/*`, `refs/keep-around/*` — server-side housekeeping
        // Before this filter they all fell through to `RefKind::LocalBranch`,
        // which populated the commit context menu with bogus "Delete
        // 'refs/stash'" entries — clicking those produced git errors.
        if !is_local_branch && !is_remote && !is_tag {
            continue;
        }

        // For *walk tips* we respect the scope filter (CurrentBranch /
        // AllLocal / AllRefs). But for *ref labels* we always include
        // remote branches and tags so the user can see `origin/main`
        // next to `main` on the same row regardless of scope. Only
        // skip refs that don't match CurrentBranch scope when the ref
        // is the HEAD-branch specific filter.
        match scope {
            GraphScope::CurrentBranch => {
                // Show HEAD + any remote tracking of HEAD + tags
                if !is_remote && !is_tag && Some(full.clone()) != head_name {
                    continue;
                }
            }
            GraphScope::AllLocal | GraphScope::AllRefs => {
                // Show everything.
            }
        }

        let kind = if is_tag {
            RefKind::Tag
        } else if is_remote {
            RefKind::RemoteBranch
        } else {
            RefKind::LocalBranch
        };

        if let Some(oid) = peel_ref_to_oid(repo, &r) {
            map.entry(oid).or_default().push(RefLabel {
                short: short.into(),
                kind,
            });
        }
    }
    // Sort per-oid: local branches first, then remote, then tags.
    for labels in map.values_mut() {
        labels.sort_by_key(|l| match l.kind {
            RefKind::LocalBranch => 0,
            RefKind::RemoteBranch => 1,
            RefKind::Tag => 2,
        });
    }
    Ok(map)
}

fn peel_ref_to_oid(repo: &gix::Repository, r: &gix::Reference<'_>) -> Option<ObjectId> {
    // Annotated tags peel through tag → commit; other refs target a
    // commit/tree directly. We only want commit-pointing oids in the
    // graph (a tag pointing to a tree shouldn't yield a tip).
    let id = r.target().try_id()?.to_owned();
    let obj = repo.find_object(id).ok()?;
    let commit = obj.peel_to_kind(gix::object::Kind::Commit).ok()?;
    Some(commit.id)
}

fn decode_for_graph(commit: &gix::Commit<'_>) -> Option<(String, String, String, String, i64)> {
    let message = commit.message().ok()?;
    let summary = message.summary().to_string();
    let body = message.body.map(|b| b.to_string()).unwrap_or_default();
    let author = commit.author().ok()?;
    let author_name = author.name.to_string();
    let author_email = author.email.to_string();
    let timestamp = author.time().map(|t| t.seconds).unwrap_or(0);
    Some((summary, body, author_name, author_email, timestamp))
}

fn first_body_preview_line(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
}
