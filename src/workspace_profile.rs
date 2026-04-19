//! Workspace-profile plumbing: team file parser, auto-detection,
//! and the resolver that picks an effective profile for a repo.
//!
//! # Where the effective profile comes from
//!
//! For any open repository we resolve to a `WorkspaceProfile` using the
//! priority chain:
//!
//! 1. Per-user override in `RepoSettings::profile_override`
//!    (written by the Settings dropdown; lives in `config.json`, **not**
//!    in the repo). This is the user deliberately saying "this is the
//!    profile I want for this checkout, regardless of what the team
//!    agreed on."
//! 2. Team file `.mergefox/workspace.toml::profile` (committed to the
//!    repo; shared across everyone who clones it). Lets a Game-dev repo
//!    nudge newcomers into Game-dev mode without each of them having to
//!    flip the setting.
//! 3. `WorkspaceProfile::default()` (General). If nothing is configured,
//!    no detection-based mode-switch has happened, and the user hasn't
//!    picked anything, we stay on the historical defaults.
//!
//! Detection (`detect_project_kind`) is **informational only** — it
//! feeds the Settings diagnostic and the startup toast, but it does
//! **not** change the effective profile on its own. Converting a
//! detection signal into a profile change is always an explicit user
//! action (clicking Switch in the toast or picking from the dropdown).
//!
//! # File location
//!
//! `.mergefox/` sits at the repository root alongside other team-shared
//! tooling files (e.g. `.editorconfig`). We keep it inside the working
//! tree rather than under `.git/` so it round-trips through clone /
//! fork / PR review the same way any other committed file does.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::WorkspaceProfile;

/// Name of the team-profile directory at the repository root.
const TEAM_DIR: &str = ".mergefox";
/// Filename inside `TEAM_DIR` that carries the team-default profile.
const TEAM_FILE: &str = "workspace.toml";

/// Parsed contents of `.mergefox/workspace.toml`.
///
/// Every field is optional because the file is optional — a repo that
/// hasn't opted into workspace profiles at all will simply not ship one,
/// and `load_team_profile` will return `TeamProfileFile::default()`.
/// Future fields (linter overrides, profile-specific knobs) land here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamProfileFile {
    /// Team-level default profile. `None` = "this repo doesn't care; use
    /// the user / application default" (General).
    #[serde(default)]
    pub profile: Option<WorkspaceProfile>,
}

/// Compute the path `.mergefox/workspace.toml` for a given repo root.
/// Returns the path regardless of whether the file actually exists, so
/// callers can offer "Create team profile file" UI keyed off the same
/// path that `load_team_profile` reads from.
pub fn team_profile_path(repo_path: &Path) -> PathBuf {
    repo_path.join(TEAM_DIR).join(TEAM_FILE)
}

/// Load the team profile file, if any.
///
/// Semantics:
/// * File missing → `Ok(TeamProfileFile::default())`. A missing file is
///   the normal state for a repo that hasn't opted in; we don't want the
///   caller to have to pattern-match on a `NotFound` error.
/// * File present but malformed TOML → `Err(_)`. This surfaces the
///   error in the Settings UI so the author of the file can fix it,
///   rather than silently ignoring a typo that would otherwise leave
///   the team mode unapplied.
pub fn load_team_profile(repo_path: &Path) -> Result<TeamProfileFile> {
    let path = team_profile_path(repo_path);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TeamProfileFile::default());
        }
        Err(err) => {
            return Err(anyhow::Error::new(err).context(format!("read {}", path.display())))
        }
    };
    // `toml` is already a dependency — reusing it keeps both team config
    // and the existing linter rules on the same serde path.
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("utf-8 decode {}", path.display()))?;
    let parsed: TeamProfileFile =
        toml::from_str(text).with_context(|| format!("parse {}", path.display()))?;
    Ok(parsed)
}

/// Combine the team file + per-user override into the effective profile.
///
/// Kept as a free function (rather than a method on `TeamProfileFile`)
/// so the priority rule lives in one obvious place and tests don't need
/// to construct a full workspace to exercise it.
pub fn effective_profile(
    team: &TeamProfileFile,
    user_override: Option<WorkspaceProfile>,
) -> WorkspaceProfile {
    if let Some(profile) = user_override {
        return profile;
    }
    if let Some(profile) = team.profile {
        return profile;
    }
    WorkspaceProfile::default()
}

/// Coarse classification of what kind of project a checkout looks like.
///
/// We keep the variant list short on purpose: the only reason we detect
/// at all is to drive a mode-switch suggestion, and a false positive on
/// a borderline case is worse than a missed detection (users can always
/// pick the profile manually). Everything in here should have a
/// single-file / single-directory marker that is conventionally only
/// present in that kind of project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedProjectKind {
    Unreal,
    Unity,
    Godot,
}

impl DetectedProjectKind {
    /// Short label for the detection diagnostic — no brand names beyond
    /// the engine itself, which is the factual marker we detected.
    pub fn label(self) -> &'static str {
        match self {
            Self::Unreal => "Unreal",
            Self::Unity => "Unity",
            Self::Godot => "Godot",
        }
    }
}

/// What `detect_project_kind` returns.
///
/// `evidence` is kept separate from `kind` so the Settings diagnostic
/// can say "Detected: Unreal (found `Project.uproject` at root)"
/// without the caller having to reconstruct which marker fired.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DetectionResult {
    pub kind: Option<DetectedProjectKind>,
    /// Human-readable evidence string. `None` when `kind` is `None`.
    pub evidence: Option<String>,
}

/// Scan the repo root for a single strong game-engine marker.
///
/// Stops at the first match; ordering is Unreal → Unity → Godot.
/// The ordering reflects decreasing marker strength: `*.uproject` at
/// root is effectively unique to Unreal; the Unity pair is also solid
/// but requires two filesystem hits; `project.godot` is unique but
/// cheap enough that it comes last.
///
/// Any IO error while walking / stat-ing is treated as "no detection"
/// — users can still pick a profile manually, and we would rather be
/// silent on a permissions edge case than show a misleading suggestion.
pub fn detect_project_kind(repo_path: &Path) -> DetectionResult {
    // --- Unreal: any `*.uproject` file at the repo root. ---------------
    // Unreal ships the main project file at the root, so a single
    // read_dir pass is enough — no recursive scan, no config parsing.
    if let Ok(entries) = std::fs::read_dir(repo_path) {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            if name
                .rsplit_once('.')
                .map(|(_, ext)| ext.eq_ignore_ascii_case("uproject"))
                .unwrap_or(false)
            {
                return DetectionResult {
                    kind: Some(DetectedProjectKind::Unreal),
                    evidence: Some(format!("found {} at root", name)),
                };
            }
        }
    }

    // --- Unity: `Assets/` + `ProjectSettings/ProjectVersion.txt`. ------
    // The `Assets/` folder alone is too generic (used by plenty of
    // non-Unity projects), so we require both. `ProjectVersion.txt`
    // under `ProjectSettings/` is written by every supported Unity
    // editor version and is the canonical tell.
    let assets = repo_path.join("Assets");
    let project_version = repo_path
        .join("ProjectSettings")
        .join("ProjectVersion.txt");
    if assets.is_dir() && project_version.is_file() {
        return DetectionResult {
            kind: Some(DetectedProjectKind::Unity),
            evidence: Some(
                "found Assets/ and ProjectSettings/ProjectVersion.txt at root".to_string(),
            ),
        };
    }

    // --- Godot: `project.godot` at the repo root. ----------------------
    let godot = repo_path.join("project.godot");
    if godot.is_file() {
        return DetectionResult {
            kind: Some(DetectedProjectKind::Godot),
            evidence: Some("found project.godot at root".to_string()),
        };
    }

    DetectionResult::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Give every test its own scratch directory so parallel runs can't
    /// see each other's marker files. `mkdtemp` would be nicer but pulls
    /// in a dependency — a counter + process id is good enough here.
    fn scratch_dir(tag: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "mergefox-wsprofile-{tag}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_team_profile_parses_game_dev() {
        let dir = scratch_dir("parse-gamedev");
        fs::create_dir_all(dir.join(".mergefox")).unwrap();
        fs::write(
            dir.join(".mergefox").join("workspace.toml"),
            b"profile = \"game_dev\"\n",
        )
        .unwrap();

        let loaded = load_team_profile(&dir).unwrap();
        assert_eq!(loaded.profile, Some(WorkspaceProfile::GameDev));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_team_profile_missing_file_is_default() {
        let dir = scratch_dir("missing");
        // No `.mergefox/workspace.toml` written — load must not fail.
        let loaded = load_team_profile(&dir).unwrap();
        assert_eq!(loaded, TeamProfileFile::default());
        assert!(loaded.profile.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_team_profile_malformed_errs() {
        let dir = scratch_dir("malformed");
        fs::create_dir_all(dir.join(".mergefox")).unwrap();
        // Quotes are unterminated — TOML parser must reject this.
        fs::write(
            dir.join(".mergefox").join("workspace.toml"),
            b"profile = \"game_dev\nstray = 1\n",
        )
        .unwrap();

        assert!(load_team_profile(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn effective_profile_priority_order() {
        // No team file, no override → default (General).
        assert_eq!(
            effective_profile(&TeamProfileFile::default(), None),
            WorkspaceProfile::General
        );

        // Team file set, no override → team wins over default.
        let team = TeamProfileFile {
            profile: Some(WorkspaceProfile::GameDev),
        };
        assert_eq!(effective_profile(&team, None), WorkspaceProfile::GameDev);

        // User override set → beats the team file even when they disagree.
        let team = TeamProfileFile {
            profile: Some(WorkspaceProfile::GameDev),
        };
        assert_eq!(
            effective_profile(&team, Some(WorkspaceProfile::General)),
            WorkspaceProfile::General
        );
    }

    #[test]
    fn detect_unreal_uproject() {
        let dir = scratch_dir("unreal");
        // Case-insensitive extension match — .UPROJECT should still
        // count, since Unreal will happily use either on Windows.
        fs::write(dir.join("MyGame.UProject"), b"").unwrap();
        let result = detect_project_kind(&dir);
        assert_eq!(result.kind, Some(DetectedProjectKind::Unreal));
        assert!(result.evidence.unwrap().contains("MyGame.UProject"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_unity_requires_both_markers() {
        let dir = scratch_dir("unity");
        // Only `Assets/` present → must NOT match Unity (too generic).
        fs::create_dir_all(dir.join("Assets")).unwrap();
        assert_eq!(detect_project_kind(&dir).kind, None);

        // Add the canonical ProjectVersion.txt → now it matches.
        fs::create_dir_all(dir.join("ProjectSettings")).unwrap();
        fs::write(
            dir.join("ProjectSettings").join("ProjectVersion.txt"),
            b"m_EditorVersion: 2022.3.0f1\n",
        )
        .unwrap();
        assert_eq!(
            detect_project_kind(&dir).kind,
            Some(DetectedProjectKind::Unity)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_godot_project_file() {
        let dir = scratch_dir("godot");
        fs::write(dir.join("project.godot"), b"config_version=5\n").unwrap();
        let result = detect_project_kind(&dir);
        assert_eq!(result.kind, Some(DetectedProjectKind::Godot));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_returns_none_for_plain_repo() {
        let dir = scratch_dir("plain");
        fs::write(dir.join("README.md"), b"# hello\n").unwrap();
        fs::write(dir.join("Cargo.toml"), b"[package]\nname=\"x\"\n").unwrap();
        assert_eq!(detect_project_kind(&dir), DetectionResult::default());
        let _ = fs::remove_dir_all(&dir);
    }
}
