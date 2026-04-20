//! Per-workspace capability gates keyed off a high-level "profile"
//! picked when the repo opens.
//!
//! The profile lets us keep the default (general-purpose) UI minimal
//! while still shipping specialised affordances for teams who need
//! them. Every gate here boils down to "should this surface be
//! visible?" — never "is this backend operation allowed?" We make the
//! gate purely cosmetic so the underlying git commands keep working
//! regardless of which profile is active; a user who switches from
//! `GameDev` to `General` doesn't lose data, they just lose the UI
//! that pointed at it.
//!
//! Current surfaces keyed off `ProfileRules`:
//!
//! * `show_lfs_lock_controls` — Git LFS lock list, context-menu
//!   lock / unlock commands, and lock-owner glyphs in the working-tree
//!   file list. `GameDev` turns these on because binary art assets
//!   (`.psd`, `.uasset`, `.fbx`) aren't mergeable and teams coming
//!   from Perforce expect a lock workflow. `General` keeps them off
//!   so text-heavy workflows aren't cluttered with a feature they
//!   don't use.

/// The set of UI surfaces a workspace profile enables. Profiles are
/// authored as "what does *this kind of team* want visible?" rather
/// than "what features exist?" — add a field here only when there's a
/// concrete surface that should be visible in some profiles and not
/// others. Don't invent hypothetical gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileRules {
    /// Render the "File locks" sidebar panel, the lock glyphs in the
    /// working-tree file list, and the Lock / Unlock / Force unlock
    /// context-menu commands. When `false`, every lock surface is
    /// omitted entirely — the LFS lock list isn't fetched at all on
    /// repo open, so we don't shell out to `git lfs locks` for users
    /// whose repos don't participate in lock workflows.
    pub show_lfs_lock_controls: bool,
}

/// The high-level profile a workspace was opened under. Currently just
/// `General` and `GameDev`, but the enum is deliberately non-exhaustive
/// in spirit so we can add (e.g.) a `Monorepo` profile without
/// renaming everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkspaceProfile {
    /// Text-heavy repos — source code, docs, config. Lock surfaces
    /// hidden because binary-asset workflows don't apply.
    #[default]
    General,
    /// Game / art asset repos where `.psd` / `.uasset` / `.fbx` live
    /// in Git LFS and a Perforce-style lock workflow is expected.
    GameDev,
}

/// Resolve the rules for a given profile. Callers use this once per
/// frame (or once per UI pass) and match on specific fields — we keep
/// `ProfileRules` `Copy` so the returned struct is cheap to clone onto
/// the stack and pass around.
pub fn rules_for(profile: WorkspaceProfile) -> ProfileRules {
    match profile {
        WorkspaceProfile::General => ProfileRules {
            show_lfs_lock_controls: false,
        },
        WorkspaceProfile::GameDev => ProfileRules {
            show_lfs_lock_controls: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{rules_for, WorkspaceProfile};

    #[test]
    fn general_profile_hides_lock_controls() {
        let rules = rules_for(WorkspaceProfile::General);
        assert!(!rules.show_lfs_lock_controls);
    }

    #[test]
    fn gamedev_profile_shows_lock_controls() {
        let rules = rules_for(WorkspaceProfile::GameDev);
        assert!(rules.show_lfs_lock_controls);
    }

    #[test]
    fn default_profile_is_general() {
        assert_eq!(WorkspaceProfile::default(), WorkspaceProfile::General);
    }
}
