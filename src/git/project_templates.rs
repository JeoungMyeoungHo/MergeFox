//! Curated `.gitattributes` / `.gitignore` templates for game-engine
//! repositories (Unreal / Unity / Godot), plus a one-click applier.
//!
//! Why this module exists
//! ----------------------
//! Game teams arrive with Unreal / Unity / Godot projects and
//! immediately bloat their Git history by committing engine-generated
//! directories (`Intermediate/`, `Library/`, `DerivedDataCache/`) and
//! by not registering binary asset types with LFS. By the time the
//! mistake is noticed the repo is already corrupted with things that
//! shouldn't be there. A big chunk of "my repo is 40 GiB and cloning
//! takes an hour" support is really "nobody ran `git lfs track *.uasset`
//! on day one".
//!
//! The fix is unglamorous: drop a well-tuned `.gitignore` and
//! `.gitattributes` next to the project file before the first commit.
//! We ship curated defaults for each detected engine and apply them in
//! one click, with a conservative merge that never stomps on a file the
//! team has already customised.
//!
//! Safety model
//! ------------
//! Stomping on a carefully-curated `.gitattributes` is worse than doing
//! nothing, so the applier is paranoid about writing to existing files:
//!
//!   * If the target file doesn't exist → write the template verbatim.
//!   * If it exists and already contains every line from the template →
//!     no-op.
//!   * If it exists and is **obviously ours to merge** (reasonably
//!     small, no alien syntax like `@@` merge markers) → append only
//!     the missing lines under a clearly-labelled `# --- MergeFox …`
//!     header so the user can see exactly what we added and revert it
//!     in `git diff` if they disagree.
//!   * If the file looks like a carefully-curated team asset
//!     (unusually large, or shows evidence of a merge conflict / other
//!     tool's format) → skip it entirely and report the path so the
//!     user can open it and apply the diff manually.
//!
//! The applier never runs `git add` — this module produces file-system
//! changes and lets the user review them in the working-tree view
//! before committing. That matches how every other "generator" action
//! in MergeFox behaves (hook installer, LFS migrator) and avoids
//! silently rewriting an untracked file the user hadn't yet noticed.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Which curated template — each engine gets one `.gitattributes` and
/// one `.gitignore`, so there are six in total. Represented as a flat
/// enum (rather than `(kind, filename)`) so UI code can key toggles,
/// translations and tests by a single discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TemplateKind {
    UnrealGitattributes,
    UnrealGitignore,
    UnityGitattributes,
    UnityGitignore,
    GodotGitattributes,
    GodotGitignore,
}

/// Detected engine that a template targets.
///
/// Mirrors the three "this is a game project" buckets the rest of the
/// app recognises. Kept local to this module so the template system
/// can stand on its own — the UI layer translates whatever project
/// kind it detected into this enum before calling `templates_for`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedProjectKind {
    Unreal,
    Unity,
    Godot,
}

/// A single curated template the UI can offer the user.
///
/// Every field is `'static` so we can keep the full set in a
/// `const`-initialised table without any allocation at startup.
pub struct TemplateDescriptor {
    pub kind: TemplateKind,
    /// Target filename relative to the repo root, always `.gitattributes`
    /// or `.gitignore` today. Carried on the descriptor (rather than
    /// derived from `kind`) so downstream code can display it without
    /// pattern-matching every variant.
    pub filename: &'static str,
    /// Human-readable engine label used in UI copy, e.g. "Unreal".
    pub project_label: &'static str,
    /// The file body we would write to a fresh repo. Must end with a
    /// newline so line-based merging produces tidy output.
    pub content: &'static str,
    /// One-line pitch surfaced next to the checkbox — tells the user
    /// what the template actually does without making them open the
    /// contents.
    pub summary: &'static str,
}

/// What happened when a single template was applied.
///
/// The variants are intentionally fine-grained so the UI can compose a
/// useful toast ("created 2 files, skipped 1 with custom content") and
/// the user can decide which paths to inspect.
#[derive(Debug, Clone)]
pub enum ApplyOutcome {
    /// The file didn't exist; we wrote the template verbatim.
    Created { path: PathBuf },
    /// The file existed and we either merged missing entries in
    /// (`added_lines > 0`) or the template was already fully present
    /// (`added_lines == 0`, a no-op).
    Merged { path: PathBuf, added_lines: usize },
    /// The file existed but looked like something we shouldn't touch
    /// automatically. `reason` is a short human-readable explanation
    /// suitable for a notification body.
    SkippedExisting { path: PathBuf, reason: String },
}

impl ApplyOutcome {
    /// Convenience for the UI — the path we acted on (or chose not to).
    pub fn path(&self) -> &Path {
        match self {
            Self::Created { path }
            | Self::Merged { path, .. }
            | Self::SkippedExisting { path, .. } => path.as_path(),
        }
    }
}

/// Return the curated template set for a detected engine. The returned
/// slice always contains exactly one `.gitattributes` and one
/// `.gitignore` entry, in that order.
pub fn templates_for(kind: DetectedProjectKind) -> &'static [TemplateDescriptor] {
    match kind {
        DetectedProjectKind::Unreal => &UNREAL_TEMPLATES,
        DetectedProjectKind::Unity => &UNITY_TEMPLATES,
        DetectedProjectKind::Godot => &GODOT_TEMPLATES,
    }
}

/// Apply one template to `repo_path`. See the module docs for the full
/// safety contract; in short, we write if the file is missing, merge
/// missing lines if it's small and unambiguous, and skip otherwise.
pub fn apply_template(repo_path: &Path, tmpl: &TemplateDescriptor) -> Result<ApplyOutcome> {
    let target = repo_path.join(tmpl.filename);

    // --- fast path: target doesn't exist, write verbatim ---------------
    //
    // We still use the atomic-write helper so a crash mid-write can't
    // leave a half-empty `.gitignore` behind (which would be worse than
    // no file at all — the user wouldn't know to re-run the applier).
    if !target.exists() {
        atomic_write(&target, tmpl.content.as_bytes())
            .with_context(|| format!("write {}", target.display()))?;
        return Ok(ApplyOutcome::Created { path: target });
    }

    // --- existing file: decide whether it's safe to merge --------------
    //
    // Read the whole file. `.gitattributes`/`.gitignore` are always
    // tiny in practice, and the hard cap below short-circuits on
    // anything unusually large before we burn memory on it.
    let existing = fs::read_to_string(&target)
        .with_context(|| format!("read existing {}", target.display()))?;

    // 16 KiB is generous for a hand-written ignore file — enough for a
    // big monorepo's ~500 curated patterns, while still rejecting a
    // `.gitignore` that has been (mis)used as a dumping ground or
    // accidentally pointed at a binary. Above this, we bail rather than
    // guess.
    const MAX_MERGE_BYTES: usize = 16 * 1024;
    if existing.len() > MAX_MERGE_BYTES {
        return Ok(ApplyOutcome::SkippedExisting {
            path: target,
            reason: format!(
                "existing file is larger than {MAX_MERGE_BYTES} bytes — review manually before applying the template"
            ),
        });
    }

    // Diff-marker detection. `@@ -x,y +a,b @@` style hunks tell us
    // we're looking at a leftover merge conflict or somebody pasted a
    // patch into the file by mistake. Either way, appending our lines
    // on top would silently produce garbage.
    if existing.contains("<<<<<<<")
        || existing.contains(">>>>>>>")
        || existing.lines().any(|l| l.starts_with("@@ "))
    {
        return Ok(ApplyOutcome::SkippedExisting {
            path: target,
            reason: "existing file contains conflict or diff markers — resolve it first".to_string(),
        });
    }

    // Compute the set of template lines not already present. We
    // compare trimmed content so trailing whitespace differences don't
    // cause spurious duplication, but we write the original untrimmed
    // template lines back (preserves the curated indentation).
    let existing_lines: std::collections::HashSet<&str> = existing
        .lines()
        .map(str::trim_end)
        .collect();

    let mut missing: Vec<&str> = Vec::new();
    for line in tmpl.content.lines() {
        let trimmed = line.trim_end();
        // Skip empty separator lines when looking for "missing" content —
        // every `.gitignore` has blank lines and we don't want the
        // merge header to claim we "added" them.
        if trimmed.is_empty() {
            continue;
        }
        if !existing_lines.contains(trimmed) {
            missing.push(line);
        }
    }

    if missing.is_empty() {
        return Ok(ApplyOutcome::Merged {
            path: target,
            added_lines: 0,
        });
    }

    // --- append missing lines under a clearly-labelled header ---------
    //
    // The header does two jobs:
    //   1. Makes the `git diff` obvious so reviewers can see this came
    //      from the templater, not from a team member hand-editing.
    //   2. Names the project kind so a later run of the applier for a
    //      different engine (e.g. a repo that hosts both Unity and
    //      Unreal subprojects) stays legible in the file.
    let mut buf = existing;
    if !buf.ends_with('\n') {
        buf.push('\n');
    }
    // A blank line before the header if the previous content didn't
    // already end with one — keeps the appended block visually distinct.
    if !buf.ends_with("\n\n") {
        buf.push('\n');
    }
    buf.push_str(&format!(
        "# --- MergeFox {} template additions ---\n",
        tmpl.project_label
    ));
    let added_lines = missing.len();
    for line in &missing {
        buf.push_str(line);
        buf.push('\n');
    }

    atomic_write(&target, buf.as_bytes())
        .with_context(|| format!("append to {}", target.display()))?;

    Ok(ApplyOutcome::Merged {
        path: target,
        added_lines,
    })
}

/// Write `bytes` to `target` by going through `<target>.mergefox-tmp`
/// and renaming on success. Rename is atomic on POSIX and on NTFS
/// within the same volume — if the process dies mid-write the original
/// file (or absence-of-file) is preserved.
fn atomic_write(target: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = match target.file_name() {
        Some(name) => {
            let mut tmp_name = name.to_os_string();
            tmp_name.push(".mergefox-tmp");
            target.with_file_name(tmp_name)
        }
        None => {
            anyhow::bail!("template target has no file name: {}", target.display());
        }
    };

    // `create` truncates any leftover `.mergefox-tmp` from a prior
    // aborted run — we own this suffix, there's no user data to save.
    {
        let mut f = fs::File::create(&tmp)
            .with_context(|| format!("create temp file {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("write temp file {}", tmp.display()))?;
        f.sync_all().ok();
    }

    fs::rename(&tmp, target)
        .with_context(|| format!("rename {} -> {}", tmp.display(), target.display()))?;
    Ok(())
}

// ============================================================
// Curated templates — one block per (engine, filename) pair.
//
// The bodies are tuned for "it is day one of your Git repo and you
// need reasonable defaults", not for "my team has special needs".
// Teams are expected to review and tweak before committing; the UI
// messaging says so explicitly.
// ============================================================

const UNREAL_GITATTRIBUTES: &str = "\
# LFS-tracked binary assets. Generated by MergeFox's project template.
# Tune to your team's preferences before committing.

# Source assets
*.uasset filter=lfs diff=lfs merge=lfs -text
*.umap   filter=lfs diff=lfs merge=lfs -text

# Common DCC / intermediate formats stored in the repo
*.fbx    filter=lfs diff=lfs merge=lfs -text
*.psd    filter=lfs diff=lfs merge=lfs -text
*.wav    filter=lfs diff=lfs merge=lfs -text
*.mp3    filter=lfs diff=lfs merge=lfs -text
*.ogg    filter=lfs diff=lfs merge=lfs -text
*.zip    filter=lfs diff=lfs merge=lfs -text

# Generated archives / binaries
*.exe    filter=lfs diff=lfs merge=lfs -text
*.dll    filter=lfs diff=lfs merge=lfs -text
*.pak    filter=lfs diff=lfs merge=lfs -text
";

const UNREAL_GITIGNORE: &str = "\
# Build artefacts / caches. Generated by MergeFox's project template.
# These directories are regenerated by the editor / build system.

Binaries/
DerivedDataCache/
Intermediate/
Saved/
.vs/
.vscode/

# Build outputs from packaging
Build/
Package/
Cooked/

# IDE
*.VC.db
*.opensdf
*.opendb
*.sdf
*.sln
*.suo
*.xcodeproj
*.xcworkspace
";

const UNITY_GITATTRIBUTES: &str = "\
# Unity serialised assets are text by default but need YAML-aware
# merge. Generated by MergeFox's project template.

*.cs diff=csharp text
*.meta text eol=lf
*.unity text eol=lf merge=unityyamlmerge
*.prefab text eol=lf merge=unityyamlmerge
*.mat text eol=lf merge=unityyamlmerge
*.asset text eol=lf merge=unityyamlmerge
*.anim text eol=lf merge=unityyamlmerge
*.controller text eol=lf merge=unityyamlmerge

# Binary assets — Unity imports these but they should never diff as text.
*.png -text filter=lfs diff=lfs merge=lfs
*.jpg -text filter=lfs diff=lfs merge=lfs
*.psd -text filter=lfs diff=lfs merge=lfs
*.tga -text filter=lfs diff=lfs merge=lfs
*.fbx -text filter=lfs diff=lfs merge=lfs
*.wav -text filter=lfs diff=lfs merge=lfs
*.mp3 -text filter=lfs diff=lfs merge=lfs
*.ogg -text filter=lfs diff=lfs merge=lfs
";

const UNITY_GITIGNORE: &str = "\
# Unity build / editor state. Generated by MergeFox's project template.

[Ll]ibrary/
[Tt]emp/
[Oo]bj/
[Bb]uild/
[Bb]uilds/
[Ll]ogs/
[Uu]ser[Ss]ettings/
[Mm]emoryCaptures/

# Editor files
*.csproj
*.unityproj
*.sln
*.suo
*.tmp
*.user
*.userprefs
*.pidb
*.booproj
*.svd
*.pdb
*.mdb
*.opendb
*.VC.db

# IDE
.vs/
.vscode/
.idea/

# Asset Store tools plugin
/[Aa]ssets/AssetStoreTools*
";

const GODOT_GITATTRIBUTES: &str = "\
# Godot 4 keeps scenes / resources as text, but binary assets need LFS
# tracking. Generated by MergeFox's project template.

*.import text eol=lf
*.tres   text eol=lf
*.tscn   text eol=lf
*.gd     text eol=lf diff=gdscript

# Typical binary asset types
*.png -text filter=lfs diff=lfs merge=lfs
*.jpg -text filter=lfs diff=lfs merge=lfs
*.wav -text filter=lfs diff=lfs merge=lfs
*.mp3 -text filter=lfs diff=lfs merge=lfs
*.ogg -text filter=lfs diff=lfs merge=lfs
*.glb -text filter=lfs diff=lfs merge=lfs
*.gltf -text filter=lfs diff=lfs merge=lfs
*.fbx -text filter=lfs diff=lfs merge=lfs
*.blend -text filter=lfs diff=lfs merge=lfs
";

const GODOT_GITIGNORE: &str = "\
# Godot-generated directories. Regenerated on import.

.godot/
.import/
export_presets.cfg
*.import
";

const UNREAL_TEMPLATES: [TemplateDescriptor; 2] = [
    TemplateDescriptor {
        kind: TemplateKind::UnrealGitattributes,
        filename: ".gitattributes",
        project_label: "Unreal",
        content: UNREAL_GITATTRIBUTES,
        summary: "LFS tracking for .uasset, .umap, .fbx, .psd, and common binary asset formats",
    },
    TemplateDescriptor {
        kind: TemplateKind::UnrealGitignore,
        filename: ".gitignore",
        project_label: "Unreal",
        content: UNREAL_GITIGNORE,
        summary: "Ignore Intermediate/, DerivedDataCache/, Saved/, and packaging outputs",
    },
];

const UNITY_TEMPLATES: [TemplateDescriptor; 2] = [
    TemplateDescriptor {
        kind: TemplateKind::UnityGitattributes,
        filename: ".gitattributes",
        project_label: "Unity",
        content: UNITY_GITATTRIBUTES,
        summary: "YAML-merge-aware line endings for scenes/prefabs, LFS for textures and audio",
    },
    TemplateDescriptor {
        kind: TemplateKind::UnityGitignore,
        filename: ".gitignore",
        project_label: "Unity",
        content: UNITY_GITIGNORE,
        summary: "Ignore Library/, Temp/, build outputs, and editor-specific IDE files",
    },
];

const GODOT_TEMPLATES: [TemplateDescriptor; 2] = [
    TemplateDescriptor {
        kind: TemplateKind::GodotGitattributes,
        filename: ".gitattributes",
        project_label: "Godot",
        content: GODOT_GITATTRIBUTES,
        summary: "Text line endings for .tscn / .tres / .gd, LFS for images, audio, and meshes",
    },
    TemplateDescriptor {
        kind: TemplateKind::GodotGitignore,
        filename: ".gitignore",
        project_label: "Godot",
        content: GODOT_GITIGNORE,
        summary: "Ignore .godot/, .import/, and export presets regenerated on project import",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_NONCE: AtomicU64 = AtomicU64::new(0);

    /// Fresh per-test tmpdir. We roll our own (no `tempfile` crate) to
    /// match the existing git-module tests and avoid a new dependency.
    fn fresh_tmpdir(tag: &str) -> PathBuf {
        let nonce = TEST_NONCE.fetch_add(1, Ordering::Relaxed);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "mergefox-templates-{tag}-{}-{nonce}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create tmpdir");
        dir
    }

    #[test]
    fn templates_for_each_kind_covers_attributes_and_ignore() {
        // Every engine must offer both files — otherwise the UI has to
        // special-case "this engine skips .gitattributes", which would
        // be more confusing than useful. Locking it here keeps the
        // curated set from regressing.
        for kind in [
            DetectedProjectKind::Unreal,
            DetectedProjectKind::Unity,
            DetectedProjectKind::Godot,
        ] {
            let set = templates_for(kind);
            assert_eq!(set.len(), 2, "{kind:?} should expose exactly two templates");
            let names: Vec<&str> = set.iter().map(|t| t.filename).collect();
            assert!(names.contains(&".gitattributes"));
            assert!(names.contains(&".gitignore"));
            assert!(set.iter().all(|t| !t.content.is_empty()));
            assert!(set.iter().all(|t| t.content.ends_with('\n')));
        }
    }

    #[test]
    fn apply_template_creates_missing_file_verbatim() {
        let dir = fresh_tmpdir("create");
        let tmpl = &templates_for(DetectedProjectKind::Unreal)[0];
        let outcome = apply_template(&dir, tmpl).expect("apply");

        match outcome {
            ApplyOutcome::Created { path } => {
                assert_eq!(path, dir.join(".gitattributes"));
                let body = fs::read_to_string(&path).expect("read");
                assert_eq!(body, tmpl.content, "fresh write must be verbatim");
            }
            other => panic!("expected Created, got {other:?}"),
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_template_is_noop_when_content_already_matches() {
        let dir = fresh_tmpdir("noop");
        let tmpl = &templates_for(DetectedProjectKind::Unity)[1]; // .gitignore
        fs::write(dir.join(tmpl.filename), tmpl.content).expect("seed");

        let outcome = apply_template(&dir, tmpl).expect("apply");
        match outcome {
            ApplyOutcome::Merged { added_lines, path } => {
                assert_eq!(added_lines, 0, "no lines to add when already applied");
                // File body should be untouched — atomic_write never ran.
                let body = fs::read_to_string(&path).expect("read");
                assert_eq!(body, tmpl.content);
            }
            other => panic!("expected Merged(0), got {other:?}"),
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_template_appends_only_missing_entries() {
        let dir = fresh_tmpdir("append");
        let tmpl = &templates_for(DetectedProjectKind::Godot)[1]; // .gitignore
        // Seed with a subset of the template and some custom lines the
        // user added. We expect both the custom lines and the existing
        // template lines to survive untouched, with only the missing
        // ones appended under the MergeFox header.
        let seed = "# our own ignores\nmy_secret.env\n.godot/\n";
        fs::write(dir.join(tmpl.filename), seed).expect("seed");

        let outcome = apply_template(&dir, tmpl).expect("apply");
        let (path, added) = match outcome {
            ApplyOutcome::Merged { path, added_lines } => (path, added_lines),
            other => panic!("expected Merged, got {other:?}"),
        };
        assert!(added > 0, "at least one line should have been appended");

        let body = fs::read_to_string(&path).expect("read");
        assert!(body.contains("my_secret.env"), "user line preserved");
        assert!(
            body.contains("# --- MergeFox Godot template additions ---"),
            "header present"
        );
        // A template line that wasn't in the seed must now appear.
        assert!(body.contains("export_presets.cfg"));
        // The already-present line must not have been appended a second
        // time — count occurrences.
        let godot_count = body.matches(".godot/\n").count();
        assert_eq!(
            godot_count, 1,
            "existing template line must not be duplicated"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_template_skips_oversize_existing_file() {
        let dir = fresh_tmpdir("oversize");
        let tmpl = &templates_for(DetectedProjectKind::Unity)[0];
        // 20 KiB > 16 KiB threshold; any content shape triggers the skip.
        let big = "x\n".repeat(10_000);
        assert!(big.len() > 16 * 1024);
        fs::write(dir.join(tmpl.filename), &big).expect("seed");

        let outcome = apply_template(&dir, tmpl).expect("apply");
        match outcome {
            ApplyOutcome::SkippedExisting { path, reason } => {
                assert_eq!(path, dir.join(tmpl.filename));
                assert!(
                    reason.contains("larger"),
                    "reason should explain the size gate: {reason}"
                );
                // And the file must be untouched.
                let body = fs::read_to_string(&path).expect("read");
                assert_eq!(body, big);
            }
            other => panic!("expected SkippedExisting, got {other:?}"),
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_template_skips_existing_with_conflict_markers() {
        // A conflict-marked file means the user is mid-merge; we must
        // not append our lines on top of `<<<<<<<` chunks and make the
        // conflict harder to resolve.
        let dir = fresh_tmpdir("conflict");
        let tmpl = &templates_for(DetectedProjectKind::Unreal)[1];
        let conflicted = "Binaries/\n<<<<<<< HEAD\nLocalOnly/\n=======\nTheirOnly/\n>>>>>>> other\n";
        fs::write(dir.join(tmpl.filename), conflicted).expect("seed");

        let outcome = apply_template(&dir, tmpl).expect("apply");
        match outcome {
            ApplyOutcome::SkippedExisting { reason, .. } => {
                assert!(reason.contains("conflict") || reason.contains("diff"));
            }
            other => panic!("expected SkippedExisting, got {other:?}"),
        }

        fs::remove_dir_all(&dir).ok();
    }
}
