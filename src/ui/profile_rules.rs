//! Centralised "what does this profile show" spec.
//!
//! Every UI module that wants to change behaviour between profiles
//! reads `rules_for(profile)` and branches on the relevant field,
//! instead of sprinkling `if profile == WorkspaceProfile::X` checks
//! through the codebase. That gives us three things:
//!
//! 1. A single place to discover *every* way a profile can change the
//!    UI. When reviewing a PR that adds a new profile variant, you only
//!    have to audit this file.
//! 2. A compile-time check that new profile variants are considered
//!    everywhere — `rules_for` pattern-matches exhaustively on the
//!    enum (no catch-all arm).
//! 3. A single seam for tests: snapshot `ProfileRules` for each profile
//!    and you have a regression fence against a future change that
//!    accidentally flips a visibility flag.
//!
//! # Phase 1 scope
//!
//! In this pass nothing *actually reads* `commit_button_label`,
//! `show_lfs_lock_controls`, etc. We're laying the plumbing so a
//! subsequent pass can wire individual UI modules (commit modal, main
//! panel, top bar) to these fields. The `General` rules therefore
//! mirror the currently-hardcoded defaults exactly — that way adding
//! the wiring later is a behaviour-neutral change.

use crate::config::WorkspaceProfile;
use crate::git::GraphScope;

/// Rules bundle consumed by UI code.
///
/// Every field answers a question of the form "should the UI show X
/// for this profile?" or "what is the default value of X for this
/// profile?". Never put free-form data here — the point of this struct
/// is to be small, cheap to copy, and trivially auditable. If a future
/// profile needs a whole extra panel's configuration, that panel should
/// grow its own config type and this struct can hold a reference /
/// enum tag pointing at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileRules {
    /// Text on the primary "save state" button in the commit modal.
    /// Changed for Game-dev so non-programmer contributors aren't
    /// asked to "Commit" (a word with a different connotation outside
    /// of source control).
    pub commit_button_label: &'static str,
    /// Whether pull-request / build-status badges show up next to
    /// branches and commits.
    pub show_ci_badges: bool,
    /// Whether interactive-rebase / split / reword menus are visible.
    /// Hidden for Game-dev on the assumption that asset-heavy history
    /// shouldn't normally be rewritten.
    pub show_advanced_rebase: bool,
    /// Whether LFS lock / unlock controls are available in the main
    /// panel. Feature is not yet implemented — the flag is a
    /// placeholder so the Game-dev profile can pre-declare intent.
    pub show_lfs_lock_controls: bool,
    /// Whether the right-hand minimap overlay on the graph is on by
    /// default. User can always toggle at runtime; this only picks
    /// the initial state.
    pub show_minimap_by_default: bool,
    /// Initial graph scope for a freshly opened repo. See
    /// [`GraphScope`] for the semantics of each variant.
    pub default_graph_scope: GraphScope,
    /// Which center-panel tab the workspace lands on when first opened.
    /// Both options are always reachable through explicit UI switches;
    /// this only sets the initial default.
    pub default_center_view: DefaultCenterView,
}

/// Possible initial tabs for the center panel. Kept a small enum rather
/// than a boolean so future profiles (e.g. a "reviewer" mode) can add
/// variants without a second boolean creeping in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultCenterView {
    /// Commit graph — the historical default.
    Graph,
    /// Project-tree / working-directory view. Not yet implemented; the
    /// Game-dev profile declares its intent to default here once the
    /// view exists.
    ProjectTree,
}

/// Look up the rules for a given profile.
///
/// Exhaustive `match` on `WorkspaceProfile` — adding a new variant to
/// the enum produces a compile error here, which is the enforcement
/// that every profile thinks about every knob.
pub fn rules_for(profile: WorkspaceProfile) -> ProfileRules {
    // Start from `General` and mutate for other variants. This makes
    // the "Game-dev differs from General in the following ways" intent
    // self-documenting and prevents the two variants from silently
    // drifting apart (e.g. someone adds a new field to General but
    // forgets to set it for Game-dev).
    let general = ProfileRules {
        commit_button_label: "Commit",
        show_ci_badges: true,
        show_advanced_rebase: true,
        show_lfs_lock_controls: false,
        show_minimap_by_default: true,
        default_graph_scope: GraphScope::CurrentBranch,
        default_center_view: DefaultCenterView::Graph,
    };
    match profile {
        WorkspaceProfile::General => general,
        WorkspaceProfile::GameDev => ProfileRules {
            commit_button_label: "Save changes",
            show_lfs_lock_controls: true,
            show_advanced_rebase: false,
            default_center_view: DefaultCenterView::ProjectTree,
            ..general
        },
        // Reserved variant — mirrors General exactly until the feature
        // lands. Having it here (rather than an unimplemented!() /
        // panic!()) keeps `rules_for` safe to call even if some future
        // path accidentally resolves a repo to `Minimal` before we've
        // fleshed it out.
        WorkspaceProfile::Minimal => general,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hard-coded defaults snapshot. When you change a default, update
    /// this test *and* whichever UI modules consume the field — that
    /// way a silent drift between the rules module and the real UI
    /// can't happen without a test edit.
    #[test]
    fn general_rules_match_current_defaults() {
        let rules = rules_for(WorkspaceProfile::General);
        assert_eq!(rules.commit_button_label, "Commit");
        assert!(rules.show_ci_badges);
        assert!(rules.show_advanced_rebase);
        assert!(!rules.show_lfs_lock_controls);
        assert!(rules.show_minimap_by_default);
        assert_eq!(rules.default_graph_scope, GraphScope::CurrentBranch);
        assert_eq!(rules.default_center_view, DefaultCenterView::Graph);
    }

    #[test]
    fn game_dev_differs_from_general_only_in_documented_fields() {
        let general = rules_for(WorkspaceProfile::General);
        let game_dev = rules_for(WorkspaceProfile::GameDev);

        // These are the four fields the profile is meant to override.
        assert_ne!(game_dev.commit_button_label, general.commit_button_label);
        assert_ne!(
            game_dev.show_advanced_rebase,
            general.show_advanced_rebase
        );
        assert_ne!(
            game_dev.show_lfs_lock_controls,
            general.show_lfs_lock_controls
        );
        assert_ne!(
            game_dev.default_center_view,
            general.default_center_view
        );

        // Everything else must stay on the General value so Phase 1
        // ships behaviourally identical to today.
        assert_eq!(game_dev.show_ci_badges, general.show_ci_badges);
        assert_eq!(
            game_dev.show_minimap_by_default,
            general.show_minimap_by_default
        );
        assert_eq!(
            game_dev.default_graph_scope,
            general.default_graph_scope
        );
    }

    #[test]
    fn minimal_mirrors_general_for_now() {
        // Reserved variant — any future change to `Minimal`'s rules
        // must also update this test so the "reserved / Coming soon"
        // contract is explicit.
        assert_eq!(
            rules_for(WorkspaceProfile::Minimal),
            rules_for(WorkspaceProfile::General)
        );
    }
}
