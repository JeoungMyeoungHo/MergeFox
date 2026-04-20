//! "Open with…" helper — routes file clicks to either a user-configured
//! DCC (digital content creation) app or the OS default viewer.
//!
//! The use-case is asset-heavy workflows: an artist sees a `.psd` in the
//! project tree, right-clicks, and wants the file to open in their
//! configured image editor. If they haven't configured anything for
//! that extension, fall back to the platform's default handler so the
//! click isn't a dead-end.
//!
//! The command template language is deliberately minimal: split on
//! whitespace into argv, substitute the token `{file}` with the
//! absolute path. Paths that contain spaces in the template itself
//! (e.g. `/Applications/My Tool.app/...`) aren't supported in v1 —
//! users with such paths should install a thin wrapper script or use
//! a shell-shim in the template. That's a pragmatic trade-off: the
//! alternative is pulling a shell-aware parser for a corner case most
//! users hit via `open -a "My Tool"` on macOS anyway.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::DccAppMappings;

/// Result of a successful launch.
#[derive(Debug, Clone)]
pub enum OpenOutcome {
    /// Spawned via a user-configured template. `command_label` is the
    /// first argv element (the binary / script that was invoked) — used
    /// in toast copy so the user knows which tool opened.
    ConfiguredApp { command_label: String },
    /// No mapping hit, or the configured mapping failed to spawn. Fell
    /// through to the platform default.
    OsDefault,
}

/// Try the configured DCC app first; on miss or spawn failure, try the
/// OS default handler. Returns an `Err` only when BOTH paths fail, so
/// the caller should treat `Ok(_)` as "file opened somewhere" and only
/// toast on `Err`.
pub fn open_file(
    repo_path: &Path,
    rel_path: &Path,
    mappings: &DccAppMappings,
) -> Result<OpenOutcome, String> {
    let absolute = repo_path.join(rel_path);
    let absolute_str = absolute.display().to_string();

    let ext = rel_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    // Configured template path.
    if let Some(ext) = ext.as_deref() {
        if let Some(template) = mappings.mappings.get(ext) {
            if !template.trim().is_empty() {
                let argv = build_argv(template, &absolute_str);
                if let Some((program, rest)) = argv.split_first() {
                    match Command::new(program).args(rest).spawn() {
                        Ok(_) => {
                            return Ok(OpenOutcome::ConfiguredApp {
                                command_label: program.clone(),
                            });
                        }
                        Err(e) => {
                            tracing::debug!(
                                target: "mergefox::open_with",
                                "configured app `{program}` failed: {e}; falling back to OS default"
                            );
                        }
                    }
                }
            }
        }
    }

    // OS default fallback. Mirrors `ui::main_panel::open_in_file_manager`
    // and the settings/about helper — the implementations are 3 lines of
    // platform-gated `Command::new`, duplicated on purpose because
    // hoisting a shared helper would require touching the error types
    // across modules for no real win.
    let status = spawn_os_default(&absolute);
    match status {
        Ok(_) => Ok(OpenOutcome::OsDefault),
        Err(e) => Err(format!("open {}: {e}", absolute.display())),
    }
}

fn spawn_os_default(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut cmd = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut cmd = Command::new("explorer");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = Command::new("xdg-open");

    cmd.arg(path);
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("spawn: {e}"))
}

/// Build the argv vector from a whitespace-split template, substituting
/// `{file}` with `path`. If no `{file}` token is present, appends `path`
/// as the final arg — matching the intuitive behaviour of editor CLIs
/// that expect the file as the last positional arg.
pub fn build_argv(template: &str, path: &str) -> Vec<String> {
    let mut out: Vec<String> = template
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();
    let mut substituted = false;
    for arg in out.iter_mut() {
        if arg == "{file}" {
            *arg = path.to_string();
            substituted = true;
        }
    }
    if !substituted {
        out.push(path.to_string());
    }
    out
}

/// Suggest seed mappings for a detected project kind. Returns a list of
/// `(extension, "")` pairs — empty command so the user sees the row and
/// can fill in their preferred app path, while the lookup still falls
/// through to the OS default until they do.
pub fn suggested_mappings_for_kind(
    kind: crate::workspace_profile::DetectedProjectKind,
) -> Vec<&'static str> {
    use crate::workspace_profile::DetectedProjectKind;
    match kind {
        DetectedProjectKind::Unreal => vec![
            "uasset", "umap", "fbx", "wav", "ogg", "png", "psd", "jpg", "tga",
        ],
        DetectedProjectKind::Unity => {
            vec!["prefab", "unity", "asset", "fbx", "png", "psd", "jpg", "tga", "wav", "ogg"]
        }
        DetectedProjectKind::Godot => vec![
            "tscn", "tres", "gd", "glb", "gltf", "fbx", "png", "wav",
        ],
    }
}

/// Convenience wrapper so the project-tree and commit-modal call sites
/// can just pass `PathBuf`s.
pub fn open_with_default(
    repo_path: &Path,
    rel_path: PathBuf,
    mappings: &DccAppMappings,
) -> Result<OpenOutcome, String> {
    open_file(repo_path, &rel_path, mappings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_argv_substitutes_file_token() {
        let argv = build_argv("/usr/bin/editor {file} --readonly", "/tmp/x.txt");
        assert_eq!(
            argv,
            vec![
                "/usr/bin/editor".to_string(),
                "/tmp/x.txt".to_string(),
                "--readonly".to_string(),
            ]
        );
    }

    #[test]
    fn build_argv_appends_when_no_token() {
        let argv = build_argv("/usr/bin/editor --new-window", "/tmp/x.txt");
        assert_eq!(
            argv,
            vec![
                "/usr/bin/editor".to_string(),
                "--new-window".to_string(),
                "/tmp/x.txt".to_string(),
            ]
        );
    }

    #[test]
    fn build_argv_handles_single_binary() {
        let argv = build_argv("blender", "/tmp/scene.blend");
        assert_eq!(
            argv,
            vec!["blender".to_string(), "/tmp/scene.blend".to_string()]
        );
    }

    #[test]
    fn build_argv_collapses_whitespace() {
        let argv = build_argv("  editor   {file}  ", "/tmp/x");
        assert_eq!(argv, vec!["editor".to_string(), "/tmp/x".to_string()]);
    }

    #[test]
    fn build_argv_supports_multiple_file_tokens() {
        // Rare but legal — a wrapper might want the file path twice.
        let argv = build_argv("diff {file} {file}", "/tmp/x");
        assert_eq!(
            argv,
            vec![
                "diff".to_string(),
                "/tmp/x".to_string(),
                "/tmp/x".to_string()
            ]
        );
    }

    #[test]
    fn open_file_missing_mapping_falls_through_to_os_default() {
        // We can't really test the spawn side-effect-free, but we CAN
        // confirm that an empty-template mapping doesn't short-circuit
        // to a configured-app success. Since there's no file, the OS
        // default will almost certainly also fail — Err is the
        // expected result on this synthetic input. What we care about
        // is NOT getting `Ok(ConfiguredApp)`.
        let mut mappings = DccAppMappings::default();
        mappings.mappings.insert("txt".into(), "".into());
        let result = open_file(Path::new("/nonexistent"), Path::new("does-not.txt"), &mappings);
        // On a sandboxed test runner `open` / `xdg-open` / `explorer`
        // may or may not succeed spawning; we only assert that we
        // never returned `ConfiguredApp`.
        if let Ok(OpenOutcome::ConfiguredApp { .. }) = result {
            panic!("empty template should not count as configured app");
        }
    }
}
