//! Per-`WorkspaceProfile` UI rules.
//!
//! The profile on `config::WorkspaceProfile` only encodes *what kind of
//! repo this is* ("general", "asset-heavy game dev", etc.). It does NOT
//! know which screen to open, which toolbar buttons to show, or whether
//! LFS lock controls should be visible — those are *UI decisions* that
//! might differ between the sidebar, the center panel, and future
//! callers. Centralising them in one place means we don't sprinkle
//! `match workspace_profile` checks across a dozen files, and the rules
//! stay discoverable: a new feature that cares about profiles only has
//! to grep for `ProfileRules` to learn what knobs exist.
//!
//! This module is intentionally read-only from the rest of the app's
//! point of view. It turns a `WorkspaceProfile` into a struct of
//! policy flags. Callers do `rules_for(ws.workspace_profile)` and read
//! the fields — they never mutate them. If you need a new knob, add a
//! field here and set it per profile in [`rules_for`].

use crate::config::WorkspaceProfile;

/// Which center-panel tab should be focused the first time a repository
/// is opened under a given profile. The tab is still user-togglable once
/// the repo is open — this only picks the initial state so that an
/// asset-heavy checkout doesn't force the user to click "Project" every
/// single time they open it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultCenterView {
    /// Commit graph (historical default).
    Graph,
    /// File-system tree, rooted at the repository working directory.
    ProjectTree,
}

/// Immutable bag of per-profile UI knobs. Populated by [`rules_for`]
/// and consumed everywhere that previously did ad-hoc `match` on the
/// profile enum.
#[derive(Debug, Clone, Copy)]
pub struct ProfileRules {
    /// Which tab in the center panel opens first on repo open.
    pub default_center_view: DefaultCenterView,
    /// Whether the UI renders Git-LFS lock / unlock controls. Only set
    /// to `true` for profiles where file locking is a routine part of
    /// the workflow — wiring the UI for locks in a general-purpose repo
    /// clutters the context menu without helping anyone.
    pub show_lfs_lock_controls: bool,
}

/// Map a profile to its rule set. Pure function; the result is cheap to
/// copy and has no interior state, so callers are free to re-invoke it
/// every frame.
pub fn rules_for(profile: WorkspaceProfile) -> ProfileRules {
    match profile {
        WorkspaceProfile::General | WorkspaceProfile::Minimal => ProfileRules {
            default_center_view: DefaultCenterView::Graph,
            show_lfs_lock_controls: false,
        },
        WorkspaceProfile::GameDev => ProfileRules {
            // Asset-heavy repos almost always navigate from "where is my
            // texture / mesh / level file?" rather than the commit graph,
            // so the project tree is the better entry point.
            default_center_view: DefaultCenterView::ProjectTree,
            // File locking is the main way binary-asset teams serialise
            // edits (two people editing the same .psd would otherwise
            // overwrite each other), so the context menu exposes it.
            show_lfs_lock_controls: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn general_profile_defaults_to_graph_and_hides_locks() {
        let r = rules_for(WorkspaceProfile::General);
        assert!(matches!(r.default_center_view, DefaultCenterView::Graph));
        assert!(!r.show_lfs_lock_controls);
    }

    #[test]
    fn gamedev_profile_opens_project_tree_and_shows_locks() {
        let r = rules_for(WorkspaceProfile::GameDev);
        assert!(matches!(
            r.default_center_view,
            DefaultCenterView::ProjectTree
        ));
        assert!(r.show_lfs_lock_controls);
    }

    #[test]
    fn minimal_profile_matches_general_for_now() {
        // Minimal is reserved for a future slimmer variant; until it
        // sprouts its own rules it should behave exactly like General.
        // If this ever diverges, the test is the signal to split them.
        let min = rules_for(WorkspaceProfile::Minimal);
        let gen = rules_for(WorkspaceProfile::General);
        assert_eq!(
            min.show_lfs_lock_controls,
            gen.show_lfs_lock_controls
        );
        assert!(matches!(
            (min.default_center_view, gen.default_center_view),
            (DefaultCenterView::Graph, DefaultCenterView::Graph)
        ));
    }
}
