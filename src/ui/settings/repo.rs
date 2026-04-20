//! "Repository" settings section — default remote, pull strategy, and
//! remote URL CRUD for the currently active tab.
//!
//! All fields save immediately:
//!
//! * Changing the default-remote combo or the pull-strategy combo writes
//!   the per-repo config entry.
//! * Editing a remote's URLs uses an explicit "Save URLs" button because
//!   saving on every keystroke would spam gix with malformed URLs
//!   during typing.
//! * Add / Delete write immediately.
//!
//! The "Project templates" panel near the top of this section is a
//! per-repo helper that drops curated `.gitattributes` / `.gitignore`
//! files for detected game-engine projects (Unreal / Unity / Godot).
//! It uses `crate::git::project_templates` and never runs `git add` —
//! the user reviews the resulting working-tree diff before committing.

use std::path::Path;

use anyhow::bail;
use egui::{Color32, ComboBox, RichText, TextEdit};

use super::{finish_repo_update, persist_config, with_settings_repo, Feedback};
use crate::app::MergeFoxApp;
use crate::config::{PullStrategyPref, RepoSettings, UiLanguage, WorkspaceProfile};
use crate::git::project_templates::{
    apply_template, templates_for, ApplyOutcome, DetectedProjectKind, TemplateKind,
};

pub fn show(ui: &mut egui::Ui, app: &mut MergeFoxApp) {
    let language = current_language(app);
    let labels = labels(language);

    ui.heading(labels.heading);
    ui.separator();

    // Render the "no repo open" empty-state and bail early. Nothing in
    // this section has meaning without an active workspace.
    let repo_path = app
        .settings_modal
        .as_ref()
        .and_then(|m| m.repo_path.clone());
    let Some(repo_path) = repo_path else {
        ui.weak(labels.no_repo_open);
        return;
    };

    ui.label(format!("{} {}", labels.current_repo, repo_path.display()));
    ui.add_space(8.0);

    let mut intent: Option<Intent> = None;

    {
        let Some(modal) = app.settings_modal.as_mut() else {
            return;
        };

        // --- default remote + pull strategy -----------------------------
        ui.horizontal(|ui| {
            ui.label(labels.default_remote);
            let before = modal.default_remote.clone();
            ComboBox::from_id_salt("settings_default_remote")
                .selected_text(
                    modal
                        .default_remote
                        .as_deref()
                        .unwrap_or(labels.auto_remote),
                )
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut modal.default_remote, None, labels.auto_remote);
                    for remote in &modal.remotes {
                        ui.selectable_value(
                            &mut modal.default_remote,
                            Some(remote.name.clone()),
                            &remote.name,
                        );
                    }
                });
            if modal.default_remote != before {
                intent = Some(Intent::SavePreferences);
            }

            ui.separator();

            ui.label(labels.pull_strategy);
            let before_strategy = modal.pull_strategy;
            ComboBox::from_id_salt("settings_pull_strategy")
                .selected_text(modal.pull_strategy.label())
                .show_ui(ui, |ui| {
                    for strategy in [
                        PullStrategyPref::Merge,
                        PullStrategyPref::Rebase,
                        PullStrategyPref::FastForwardOnly,
                    ] {
                        ui.selectable_value(&mut modal.pull_strategy, strategy, strategy.label());
                    }
                });
            if modal.pull_strategy != before_strategy {
                intent = Some(Intent::SavePreferences);
            }
        });
        ui.weak(labels.repo_settings_hint);

        // --- per-repo account selection --------------------------------
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(labels.provider_account);
            let accounts = &app.config.provider_accounts;
            // Current selection slug lives on `modal`; we mirror it from
            // RepoSettings on open and write back on change.
            let before_account = modal.provider_account_slug.clone();
            let display = modal
                .provider_account_slug
                .as_deref()
                .and_then(|slug| {
                    accounts
                        .iter()
                        .find(|a| a.id.slug() == slug)
                        .map(|a| format!("{} ({})", a.display_name, a.id.kind.slug()))
                })
                .unwrap_or_else(|| labels.auto_account.to_string());
            ComboBox::from_id_salt("settings_repo_account")
                .selected_text(display)
                .show_ui(ui, |ui| {
                    // "Auto" = detect from remote URL host.
                    if ui
                        .selectable_label(
                            modal.provider_account_slug.is_none(),
                            labels.auto_account,
                        )
                        .clicked()
                    {
                        modal.provider_account_slug = None;
                    }
                    ui.separator();
                    for acc in accounts {
                        let slug = acc.id.slug();
                        let selected =
                            modal.provider_account_slug.as_deref() == Some(slug.as_str());
                        let label = format!(
                            "{}  ({}, {})",
                            acc.display_name,
                            acc.id.kind.slug(),
                            acc.id.username
                        );
                        if ui.selectable_label(selected, label).clicked() {
                            modal.provider_account_slug = Some(slug);
                        }
                    }
                });
            if modal.provider_account_slug != before_account {
                intent = Some(Intent::SavePreferences);
            }
        });
        ui.weak(labels.provider_account_hint);

        // --- project templates ----------------------------------------
        //
        // Detected game-engine projects get a curated `.gitattributes`
        // / `.gitignore` pair in one click. The detection runs here
        // (rather than in a shared cache) because the section is
        // rendered at most a few times per second and the probe is a
        // handful of `Path::exists` calls — cheaper than invalidating
        // a workspace-level cache when the repo's files change.
        //
        // Rendering reads/writes modal state through plain fields so we
        // don't need a second `settings_modal.as_mut()` borrow from
        // inside the already-active one.
        let detected = detect_project_kind(&repo_path);
        if let Some(kind) = detected {
            let (next_expanded, next_selection, fire_intent) = render_project_templates(
                ui,
                &repo_path,
                kind,
                modal.project_templates_expanded,
                modal.project_template_selection.clone(),
                &labels,
            );
            modal.project_templates_expanded = next_expanded;
            modal.project_template_selection = next_selection;
            if let Some(tmpl_intent) = fire_intent {
                intent = Some(tmpl_intent);
            }
        }

        ui.add_space(12.0);
        ui.heading(labels.remote_urls);
        ui.separator();
        if modal.remotes.is_empty() {
            ui.weak(labels.no_remotes);
        } else {
            for remote in &mut modal.remotes {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&remote.name).strong());
                        if modal.default_remote.as_deref() == Some(remote.name.as_str()) {
                            ui.label(RichText::new(labels.default_badge).weak());
                        }
                    });
                    ui.label(labels.fetch_url);
                    ui.add(
                        TextEdit::singleline(&mut remote.fetch_url).desired_width(f32::INFINITY),
                    );
                    ui.label(labels.push_url);
                    ui.add(
                        TextEdit::singleline(&mut remote.push_url)
                            .desired_width(f32::INFINITY)
                            .hint_text(labels.push_url_placeholder),
                    );
                    // Rename row — kept separate from URL edits because
                    // `git remote rename` also rewrites upstream tracking
                    // config, which is not what users expect from "Save
                    // URLs".
                    ui.horizontal(|ui| {
                        ui.label(labels.rename_to);
                        ui.add(
                            TextEdit::singleline(&mut remote.rename_to)
                                .desired_width(160.0)
                                .hint_text(&remote.name),
                        );
                        let new_name = remote.rename_to.trim().to_string();
                        let ok = !new_name.is_empty() && new_name != remote.name;
                        ui.add_enabled_ui(ok, |ui| {
                            if ui.button(labels.rename_remote).clicked() {
                                intent = Some(Intent::RenameRemote {
                                    old_name: remote.name.clone(),
                                    new_name,
                                });
                            }
                        });
                    });
                    ui.horizontal(|ui| {
                        if ui.button(labels.save_remote).clicked() {
                            intent = Some(Intent::SaveRemote {
                                name: remote.name.clone(),
                                fetch_url: remote.fetch_url.clone(),
                                push_url: remote.push_url.clone(),
                            });
                        }
                        if ui
                            .button(RichText::new(labels.delete_remote).color(Color32::LIGHT_RED))
                            .clicked()
                        {
                            intent = Some(Intent::DeleteRemote {
                                name: remote.name.clone(),
                            });
                        }
                    });
                });
                ui.add_space(6.0);
            }
        }

        ui.add_space(12.0);
        ui.heading(labels.add_remote);
        ui.separator();
        ui.label(labels.remote_name);
        ui.add(
            TextEdit::singleline(&mut modal.new_remote_name)
                .desired_width(f32::INFINITY)
                .hint_text("origin"),
        );
        ui.label(labels.fetch_url);
        ui.add(
            TextEdit::singleline(&mut modal.new_fetch_url)
                .desired_width(f32::INFINITY)
                .hint_text("https://example.com/owner/repo.git"),
        );
        ui.label(labels.push_url);
        ui.add(
            TextEdit::singleline(&mut modal.new_push_url)
                .desired_width(f32::INFINITY)
                .hint_text(labels.push_url_placeholder),
        );
        if ui.button(labels.add_remote_button).clicked() {
            intent = Some(Intent::AddRemote {
                name: modal.new_remote_name.clone(),
                fetch_url: modal.new_fetch_url.clone(),
                push_url: modal.new_push_url.clone(),
            });
        }

        // --- worktrees --------------------------------------------------
        //
        // Lazy-load on first render of this settings section: the list is
        // read-only state so we can just re-query on each open rather
        // than cache-invalidate it through the full ws cache pipeline.
        ui.add_space(16.0);
        ui.heading(labels.worktrees);
        ui.separator();
        if modal.worktrees.is_none() {
            intent = intent.or(Some(Intent::RefreshWorktrees));
        }
        match &modal.worktrees {
            Some(list) if list.is_empty() => {
                ui.weak(labels.no_worktrees);
            }
            Some(list) => {
                for wt in list {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(wt.path.display().to_string()).strong());
                            if wt.is_main {
                                ui.label(RichText::new(labels.main_badge).weak());
                            }
                            if wt.is_locked {
                                ui.label(RichText::new(labels.locked_badge).weak());
                            }
                            if wt.is_prunable {
                                ui.colored_label(
                                    Color32::from_rgb(240, 180, 96),
                                    labels.prunable_badge,
                                );
                            }
                        });
                        if let Some(branch) = &wt.branch {
                            ui.weak(format!("branch: {branch}"));
                        } else if wt.is_detached {
                            ui.weak("detached HEAD");
                        }
                        if !wt.is_main {
                            ui.horizontal(|ui| {
                                if ui
                                    .button(
                                        RichText::new(labels.remove_worktree)
                                            .color(Color32::LIGHT_RED),
                                    )
                                    .clicked()
                                {
                                    intent = Some(Intent::RemoveWorktree {
                                        path: wt.path.clone(),
                                        force: false,
                                    });
                                }
                                if ui.button(labels.remove_worktree_force).clicked() {
                                    intent = Some(Intent::RemoveWorktree {
                                        path: wt.path.clone(),
                                        force: true,
                                    });
                                }
                            });
                        }
                    });
                    ui.add_space(4.0);
                }
            }
            None => {
                ui.weak(labels.loading);
            }
        }

        // --- maintenance -----------------------------------------------
        //
        // fsck / gc / repack. Kept at the bottom because they're power
        // tools — the user should have scrolled past the common-case
        // remote + worktree sections to find them. Status above each
        // button so the user sees "2 packs, 128 KiB loose" before
        // deciding.
        ui.add_space(16.0);
        ui.heading(labels.maintenance);
        ui.separator();
        if modal.count_objects.is_none() {
            intent = intent.or(Some(Intent::RefreshCountObjects));
        }
        if let Some(summary) = modal.count_objects.as_ref() {
            let loose = summary.size_kib.unwrap_or(0);
            let packed = summary.size_pack_kib.unwrap_or(0);
            let packs = summary.packs.unwrap_or(0);
            ui.weak(format!(
                "{} KiB loose · {} KiB packed · {} pack{}",
                loose,
                packed,
                packs,
                if packs == 1 { "" } else { "s" }
            ));
            if summary.suggests_repack() {
                ui.colored_label(Color32::from_rgb(240, 180, 96), labels.repack_recommended);
            }
            ui.add_space(4.0);
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .button(labels.run_fsck)
                .on_hover_text(labels.fsck_hint)
                .clicked()
            {
                intent = Some(Intent::RunFsck);
            }
            if ui
                .button(labels.run_gc)
                .on_hover_text(labels.gc_hint)
                .clicked()
            {
                intent = Some(Intent::RunGc { aggressive: false });
            }
            if ui
                .button(labels.run_gc_aggressive)
                .on_hover_text(labels.gc_aggressive_hint)
                .clicked()
            {
                intent = Some(Intent::RunGc { aggressive: true });
            }
            if ui
                .button(labels.run_repack)
                .on_hover_text(labels.repack_hint)
                .clicked()
            {
                intent = Some(Intent::RunRepack);
            }
        });

        // --- sparse checkout -------------------------------------------
        //
        // Cone mode only. Classic (pattern-based) mode is read-only if
        // someone else configured it — `cone=false && enabled` shows a
        // warning and hides the edit controls rather than offering to
        // translate between the two schemas.
        ui.add_space(16.0);
        ui.heading(labels.sparse_checkout);
        ui.separator();
        if modal.sparse_checkout.is_none() {
            intent = intent.or(Some(Intent::RefreshSparseCheckout));
        }
        if let Some(status) = modal.sparse_checkout.clone() {
            if status.enabled && !status.cone {
                ui.colored_label(Color32::from_rgb(240, 180, 96), labels.sparse_classic_mode);
                ui.weak(labels.sparse_classic_hint);
                for p in &status.patterns {
                    ui.label(egui::RichText::new(p).monospace());
                }
            } else {
                ui.weak(labels.sparse_intro);
                ui.label(labels.sparse_patterns_label);
                ui.add(
                    TextEdit::multiline(&mut modal.sparse_patterns_draft)
                        .desired_rows(5)
                        .desired_width(f32::INFINITY)
                        .hint_text("src/\ncrates/engine/\ndocs/"),
                );
                ui.horizontal(|ui| {
                    if ui
                        .button(if status.enabled {
                            labels.sparse_apply
                        } else {
                            labels.sparse_enable
                        })
                        .on_hover_text(labels.sparse_apply_hint)
                        .clicked()
                    {
                        let patterns: Vec<String> = modal
                            .sparse_patterns_draft
                            .lines()
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        intent = Some(Intent::ApplySparseCheckout { patterns });
                    }
                    if status.enabled
                        && ui
                            .button(labels.sparse_disable)
                            .on_hover_text(labels.sparse_disable_hint)
                            .clicked()
                    {
                        intent = Some(Intent::DisableSparseCheckout);
                    }
                });
            }
        } else {
            ui.weak(labels.loading);
        }

        // --- submodules -----------------------------------------------
        ui.add_space(16.0);
        ui.heading(labels.submodules);
        ui.separator();
        if modal.submodules.is_none() {
            intent = intent.or(Some(Intent::RefreshSubmodules));
        }
        match &modal.submodules {
            Some(list) if list.is_empty() => {
                ui.weak(labels.no_submodules);
            }
            Some(list) => {
                use crate::git::SubmoduleState;
                for sm in list {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&sm.path).strong().monospace());
                            let (badge_color, badge_text) = match sm.state {
                                SubmoduleState::InSync => {
                                    (Color32::from_rgb(116, 192, 136), labels.submodule_in_sync)
                                }
                                SubmoduleState::NotInitialised => {
                                    (Color32::from_rgb(148, 170, 210), labels.submodule_not_init)
                                }
                                SubmoduleState::Modified => {
                                    (Color32::from_rgb(240, 180, 96), labels.submodule_modified)
                                }
                                SubmoduleState::Conflict => {
                                    (Color32::from_rgb(235, 108, 108), labels.submodule_conflict)
                                }
                            };
                            ui.colored_label(badge_color, badge_text);
                        });
                        ui.weak(format!(
                            "SHA: {}{}",
                            sm.sha.chars().take(12).collect::<String>(),
                            sm.described
                                .as_deref()
                                .map(|d| format!("  ({d})"))
                                .unwrap_or_default()
                        ));
                        ui.horizontal(|ui| {
                            if ui.small_button(labels.submodule_update).clicked() {
                                intent = Some(Intent::UpdateSubmodule {
                                    path: Some(sm.path.clone()),
                                });
                            }
                            if ui.small_button(labels.submodule_sync).clicked() {
                                intent = Some(Intent::SyncSubmodule {
                                    path: Some(sm.path.clone()),
                                });
                            }
                        });
                    });
                    ui.add_space(4.0);
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button(labels.submodule_update_all).clicked() {
                        intent = Some(Intent::UpdateSubmodule { path: None });
                    }
                    if ui.button(labels.submodule_sync_all).clicked() {
                        intent = Some(Intent::SyncSubmodule { path: None });
                    }
                });
            }
            None => {
                ui.weak(labels.loading);
            }
        }
    }

    if let Some(intent) = intent {
        handle_intent(app, intent, &labels);
    }

    // Rendered after the modal's mut-borrow block closes so the
    // section can freely mutate `app.config` + live workspace state.
    render_workspace_profile_section(ui, app);
    render_dcc_apps_section(ui, app);
}

enum Intent {
    SavePreferences,
    SaveRemote {
        name: String,
        fetch_url: String,
        push_url: String,
    },
    DeleteRemote {
        name: String,
    },
    AddRemote {
        name: String,
        fetch_url: String,
        push_url: String,
    },
    RenameRemote {
        old_name: String,
        new_name: String,
    },
    RefreshWorktrees,
    RemoveWorktree {
        path: std::path::PathBuf,
        force: bool,
    },
    RefreshCountObjects,
    RunFsck,
    RunGc {
        aggressive: bool,
    },
    RunRepack,
    RefreshSparseCheckout,
    ApplySparseCheckout {
        patterns: Vec<String>,
    },
    DisableSparseCheckout,
    RefreshSubmodules,
    UpdateSubmodule {
        path: Option<String>,
    },
    SyncSubmodule {
        path: Option<String>,
    },
    /// Apply the user's currently-checked subset of `templates_for(kind)`
    /// to the repo at `repo_path`. Dispatched from the "Project
    /// templates" sub-section.
    ApplyProjectTemplates {
        repo_path: std::path::PathBuf,
        kind: DetectedProjectKind,
        selected: Vec<TemplateKind>,
    },
}

fn handle_intent(app: &mut MergeFoxApp, intent: Intent, labels: &Labels) {
    match intent {
        Intent::SavePreferences => save_preferences(app, labels),
        Intent::SaveRemote {
            name,
            fetch_url,
            push_url,
        } => update_remote(app, &name, &fetch_url, &push_url, labels),
        Intent::DeleteRemote { name } => delete_remote(app, &name, labels),
        Intent::AddRemote {
            name,
            fetch_url,
            push_url,
        } => add_remote(app, &name, &fetch_url, &push_url, labels),
        Intent::RenameRemote { old_name, new_name } => {
            rename_remote(app, &old_name, &new_name, labels)
        }
        Intent::RefreshWorktrees => refresh_worktrees(app),
        Intent::RemoveWorktree { path, force } => remove_worktree(app, &path, force, labels),
        Intent::RefreshCountObjects => refresh_count_objects(app),
        Intent::RunFsck => run_fsck(app, labels),
        Intent::RunGc { aggressive } => run_gc(app, aggressive, labels),
        Intent::RunRepack => run_repack(app, labels),
        Intent::RefreshSparseCheckout => refresh_sparse_checkout(app),
        Intent::ApplySparseCheckout { patterns } => apply_sparse_checkout(app, patterns, labels),
        Intent::DisableSparseCheckout => disable_sparse_checkout(app, labels),
        Intent::RefreshSubmodules => refresh_submodules(app),
        Intent::UpdateSubmodule { path } => update_submodule(app, path, labels),
        Intent::SyncSubmodule { path } => sync_submodule(app, path, labels),
        Intent::ApplyProjectTemplates {
            repo_path,
            kind,
            selected,
        } => apply_project_templates(app, &repo_path, kind, &selected, labels),
    }
}

fn refresh_submodules(app: &mut MergeFoxApp) {
    let result = with_settings_repo(app, |repo| repo.submodule_status());
    if let Some(modal) = app.settings_modal.as_mut() {
        modal.submodules = Some(result.unwrap_or_default());
    }
}

fn update_submodule(app: &mut MergeFoxApp, path: Option<String>, labels: &Labels) {
    let target = path.clone();
    let result = with_settings_repo(app, move |repo| repo.submodule_update(target.as_deref()));
    match result {
        Ok(_) => {
            app.notify_ok(match path {
                Some(p) => format!("{}: {p}", labels.submodule_updated),
                None => labels.submodule_updated_all.to_string(),
            });
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.submodules = None;
            }
        }
        Err(err) => app.notify_err_with_detail(labels.submodule_update_failed, format!("{err:#}")),
    }
}

fn sync_submodule(app: &mut MergeFoxApp, path: Option<String>, labels: &Labels) {
    let target = path.clone();
    let result = with_settings_repo(app, move |repo| repo.submodule_sync(target.as_deref()));
    match result {
        Ok(_) => {
            app.notify_ok(match path {
                Some(p) => format!("{}: {p}", labels.submodule_synced),
                None => labels.submodule_synced_all.to_string(),
            });
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.submodules = None;
            }
        }
        Err(err) => app.notify_err_with_detail(labels.submodule_sync_failed, format!("{err:#}")),
    }
}

fn refresh_sparse_checkout(app: &mut MergeFoxApp) {
    let status = with_settings_repo(app, |repo| Ok(repo.sparse_checkout_status()));
    if let Some(modal) = app.settings_modal.as_mut() {
        match status {
            Ok(s) => {
                // Seed the draft textarea with the current patterns so
                // the user edits the live set rather than starting from
                // an empty box.
                modal.sparse_patterns_draft = s.patterns.join("\n");
                modal.sparse_checkout = Some(s);
            }
            Err(_) => {
                modal.sparse_checkout = Some(crate::git::SparseCheckoutStatus::default());
            }
        }
    }
}

fn apply_sparse_checkout(app: &mut MergeFoxApp, patterns: Vec<String>, labels: &Labels) {
    let result = with_settings_repo(app, |repo| repo.sparse_checkout_enable_cone(&patterns));
    match result {
        Ok(()) => {
            app.notify_ok(labels.sparse_applied);
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.sparse_checkout = None; // force re-read
            }
        }
        Err(err) => {
            app.notify_err_with_detail(labels.sparse_apply_failed, format!("{err:#}"));
        }
    }
}

fn disable_sparse_checkout(app: &mut MergeFoxApp, labels: &Labels) {
    let result = with_settings_repo(app, |repo| repo.sparse_checkout_disable());
    match result {
        Ok(()) => {
            app.notify_ok(labels.sparse_disabled);
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.sparse_checkout = None;
            }
        }
        Err(err) => {
            app.notify_err_with_detail(labels.sparse_disable_failed, format!("{err:#}"));
        }
    }
}

fn refresh_count_objects(app: &mut MergeFoxApp) {
    let result = with_settings_repo(app, |repo| repo.count_objects());
    if let Some(modal) = app.settings_modal.as_mut() {
        modal.count_objects = match result {
            Ok(s) => Some(s),
            Err(_) => Some(crate::git::CountObjectsSummary::default()),
        };
    }
}

// fsck / gc / repack run synchronously today. Typical repos finish in
// sub-second and the UI freeze is preferable to the complexity of a
// per-op cancel+progress job right now; see TODO/production.md §E12
// for the follow-up that generalises GitJob beyond network ops.
fn run_fsck(app: &mut MergeFoxApp, labels: &Labels) {
    let result = with_settings_repo(app, |repo| repo.fsck());
    match result {
        Ok(text) => {
            let detail = if text.trim().is_empty() {
                labels.fsck_clean.to_string()
            } else {
                text
            };
            app.notify_ok(labels.fsck_done);
            app.notifications.push_with_detail(
                crate::ui::notifications::NotifSeverity::Info,
                labels.fsck_done,
                Some(detail),
            );
        }
        Err(err) => {
            app.notify_err_with_detail(labels.fsck_failed, format!("{err:#}"));
        }
    }
}

fn run_gc(app: &mut MergeFoxApp, aggressive: bool, labels: &Labels) {
    let result = with_settings_repo(app, |repo| repo.gc(aggressive));
    match result {
        Ok(text) => {
            app.notify_ok(if aggressive {
                labels.gc_aggressive_done
            } else {
                labels.gc_done
            });
            if !text.trim().is_empty() {
                tracing::info!(target: "mergefox::maintenance", output = %text, "gc");
            }
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.count_objects = None; // force re-read
            }
        }
        Err(err) => {
            app.notify_err_with_detail(labels.gc_failed, format!("{err:#}"));
        }
    }
}

fn run_repack(app: &mut MergeFoxApp, labels: &Labels) {
    let result = with_settings_repo(app, |repo| repo.repack());
    match result {
        Ok(text) => {
            app.notify_ok(labels.repack_done);
            if !text.trim().is_empty() {
                tracing::info!(target: "mergefox::maintenance", output = %text, "repack");
            }
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.count_objects = None;
            }
        }
        Err(err) => {
            app.notify_err_with_detail(labels.repack_failed, format!("{err:#}"));
        }
    }
}

fn refresh_worktrees(app: &mut MergeFoxApp) {
    let result = with_settings_repo(app, |repo| repo.list_worktrees());
    if let Some(modal) = app.settings_modal.as_mut() {
        match result {
            Ok(list) => modal.worktrees = Some(list),
            Err(_) => modal.worktrees = Some(Vec::new()),
        }
    }
}

fn remove_worktree(app: &mut MergeFoxApp, path: &std::path::Path, force: bool, labels: &Labels) {
    let path = path.to_path_buf();
    let result = with_settings_repo(app, |repo| repo.remove_worktree(&path, force));
    match result {
        Ok(()) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::ok(labels.removed_worktree.to_string()));
                modal.worktrees = None; // force reload
            }
            app.hud = Some(crate::app::Hud::new(labels.removed_worktree, 1600));
        }
        Err(err) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::err(format!("{err:#}")));
            }
        }
    }
}

fn rename_remote(app: &mut MergeFoxApp, old_name: &str, new_name: &str, labels: &Labels) {
    let result = with_settings_repo(app, |repo| {
        let trimmed = new_name.trim();
        if trimmed.is_empty() {
            bail!("new remote name cannot be empty");
        }
        if trimmed.contains(char::is_whitespace) {
            bail!("remote names cannot contain whitespace");
        }
        repo.rename_remote(old_name, trimmed)
    });
    // If the default-remote pointed at the old name, migrate it so the
    // rename doesn't silently break push/pull for this repo.
    if result.is_ok() {
        if let Some(modal) = app.settings_modal.as_mut() {
            if modal.default_remote.as_deref() == Some(old_name) {
                modal.default_remote = Some(new_name.trim().to_string());
            }
            // Persist the provider_account / default_remote change.
        }
        save_preferences(app, labels);
    }
    finish_repo_update(app, result, labels.renamed_remote);
}

fn save_preferences(app: &mut MergeFoxApp, labels: &Labels) {
    let (repo_path, settings) = {
        let Some(modal) = app.settings_modal.as_ref() else {
            return;
        };
        let Some(repo_path) = modal.repo_path.clone() else {
            return;
        };
        let settings = RepoSettings {
            default_remote: modal.default_remote.clone(),
            pull_strategy: modal.pull_strategy,
            provider_account: modal.provider_account_slug.clone(),
            profile_override: app
                .config
                .repo_settings
                .get(&repo_path.to_string_lossy().to_string())
                .and_then(|s| s.profile_override),
        };
        (repo_path, settings)
    };

    app.config.set_repo_settings(&repo_path, settings);
    persist_config(app, labels.saved_prefs);
}

fn update_remote(
    app: &mut MergeFoxApp,
    name: &str,
    fetch_url: &str,
    push_url: &str,
    labels: &Labels,
) {
    let result = with_settings_repo(app, |repo| {
        let fetch_url = fetch_url.trim();
        if fetch_url.is_empty() {
            bail!("fetch URL cannot be empty");
        }
        let push_url = cleaned(push_url);
        repo.update_remote_urls(name, fetch_url, push_url.as_deref())
    });
    finish_repo_update(app, result, labels.updated_remote);
}

fn delete_remote(app: &mut MergeFoxApp, name: &str, labels: &Labels) {
    let result = with_settings_repo(app, |repo| repo.delete_remote(name));
    if result.is_ok() {
        if let Some(modal) = app.settings_modal.as_mut() {
            if modal.default_remote.as_deref() == Some(name) {
                modal.default_remote = None;
            }
        }
    }
    finish_repo_update(app, result, labels.deleted_remote);
}

fn add_remote(app: &mut MergeFoxApp, name: &str, fetch_url: &str, push_url: &str, labels: &Labels) {
    let result = with_settings_repo(app, |repo| {
        let name = name.trim();
        let fetch_url = fetch_url.trim();
        if name.is_empty() {
            bail!("remote name cannot be empty");
        }
        if fetch_url.is_empty() {
            bail!("fetch URL cannot be empty");
        }
        let push_url = cleaned(push_url);
        repo.add_remote(name, fetch_url, push_url.as_deref())
    });

    match result {
        Ok(()) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                if modal.default_remote.is_none() {
                    modal.default_remote = Some(name.trim().to_string());
                }
                modal.new_remote_name.clear();
                modal.new_fetch_url.clear();
                modal.new_push_url.clear();
            }
            finish_repo_update(app, Ok(()), labels.added_remote);
        }
        Err(err) => {
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.feedback = Some(Feedback::err(format!("{err:#}")));
            }
        }
    }
}

fn cleaned(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Heuristic probe: does this repo look like an Unreal / Unity / Godot
/// project? We check the repo root for a handful of well-known marker
/// files/directories and stop at the first match. `None` means no
/// engine was detected; in that case the UI hides the template panel.
///
/// The markers are intentionally conservative — we'd rather fail to
/// detect a genuine game project than mislabel a generic Rust repo as
/// "Unreal" and offer to drop 40 ignore patterns on it. The UI also
/// surfaces the detected engine to the user before any template runs,
/// so a mis-detection can't cause damage without a confirmation click.
fn detect_project_kind(repo_path: &Path) -> Option<DetectedProjectKind> {
    // Godot first — its `project.godot` marker is the most specific of
    // the three (one file, a fixed name). Unity's `ProjectSettings/`
    // and Unreal's `.uproject` come next; neither directory name is
    // guaranteed to be unique in the wild so we also require a sibling
    // that Unity / Unreal typically generate.
    if repo_path.join("project.godot").is_file() {
        return Some(DetectedProjectKind::Godot);
    }
    if has_uproject(repo_path) {
        return Some(DetectedProjectKind::Unreal);
    }
    if repo_path.join("ProjectSettings").is_dir() && repo_path.join("Assets").is_dir() {
        return Some(DetectedProjectKind::Unity);
    }
    None
}

/// Unreal doesn't have a fixed `.uproject` filename — it takes the
/// project's name — so we scan the repo root for any `*.uproject`.
/// Root-only is fine because Unreal refuses to open a project whose
/// `.uproject` file lives in a subdirectory without a manual move.
fn has_uproject(repo_path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(repo_path) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".uproject") {
            return true;
        }
    }
    false
}

/// Render the "Project templates" block. Returns the updated
/// (expanded, selection, optional-apply-intent) so the caller can
/// write them back to the modal without a nested `as_mut` borrow.
///
/// The function is pure w.r.t. application state — it reads the
/// current panel pose through arguments, composes egui widgets, and
/// surfaces the user's decisions as return values. All persistence
/// happens at the call site.
fn render_project_templates(
    ui: &mut egui::Ui,
    repo_path: &Path,
    kind: DetectedProjectKind,
    mut expanded: bool,
    selection_in: std::collections::HashMap<TemplateKind, bool>,
    labels: &Labels,
) -> (
    bool,
    std::collections::HashMap<TemplateKind, bool>,
    Option<Intent>,
) {
    let templates = templates_for(kind);

    ui.add_space(16.0);
    ui.heading(labels.project_templates);
    ui.separator();
    ui.horizontal(|ui| {
        ui.label(labels.project_templates_detected);
        ui.label(
            RichText::new(project_kind_label(kind))
                .strong()
                .color(Color32::from_rgb(140, 200, 155)),
        );
    });
    ui.weak(labels.project_templates_blurb);

    // Seed the checkbox map on first expand so every template starts
    // ticked — the most common flow is "apply everything we suggest".
    let mut selection = if selection_in.is_empty() {
        let mut map = std::collections::HashMap::new();
        for tmpl in templates {
            map.insert(tmpl.kind, true);
        }
        map
    } else {
        selection_in
    };

    let header = if expanded {
        labels.project_templates_collapse
    } else {
        labels.project_templates_review
    };
    if ui.button(header).clicked() {
        expanded = !expanded;
    }

    let mut fire_intent: Option<Intent> = None;
    if expanded {
        ui.add_space(4.0);
        for tmpl in templates {
            let entry = selection.entry(tmpl.kind).or_insert(true);
            ui.horizontal(|ui| {
                ui.checkbox(entry, "");
                ui.label(RichText::new(tmpl.filename).monospace().strong());
                ui.weak(format!("— {}", tmpl.summary));
            });
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let any_selected = selection.values().any(|v| *v);
            if ui
                .add_enabled(any_selected, egui::Button::new(labels.project_templates_apply))
                .on_hover_text(labels.project_templates_apply_hint)
                .clicked()
            {
                let chosen: Vec<TemplateKind> = templates
                    .iter()
                    .filter(|t| selection.get(&t.kind).copied().unwrap_or(true))
                    .map(|t| t.kind)
                    .collect();
                fire_intent = Some(Intent::ApplyProjectTemplates {
                    repo_path: repo_path.to_path_buf(),
                    kind,
                    selected: chosen,
                });
            }
            if ui.button(labels.project_templates_cancel).clicked() {
                expanded = false;
            }
        });
    }

    (expanded, selection, fire_intent)
}


fn project_kind_label(kind: DetectedProjectKind) -> &'static str {
    match kind {
        DetectedProjectKind::Unreal => "Unreal",
        DetectedProjectKind::Unity => "Unity",
        DetectedProjectKind::Godot => "Godot",
    }
}

/// Apply the user-chosen subset and surface a single summary toast.
///
/// We compose *one* notification (not one per template) because the
/// common case is "created 2 files" and a flurry of toasts for a
/// 2-item operation feels noisy. Per-path details go into the
/// notification's expandable detail body.
fn apply_project_templates(
    app: &mut MergeFoxApp,
    repo_path: &Path,
    kind: DetectedProjectKind,
    selected: &[TemplateKind],
    labels: &Labels,
) {
    let templates = templates_for(kind);
    let mut created = 0usize;
    let mut merged_with_additions = 0usize;
    let mut merged_no_op = 0usize;
    let mut skipped: Vec<(std::path::PathBuf, String)> = Vec::new();
    let mut errors: Vec<(String, String)> = Vec::new();
    let mut detail_lines: Vec<String> = Vec::new();

    for tmpl in templates {
        if !selected.contains(&tmpl.kind) {
            continue;
        }
        match apply_template(repo_path, tmpl) {
            Ok(ApplyOutcome::Created { path }) => {
                created += 1;
                detail_lines.push(format!("created {}", path.display()));
            }
            Ok(ApplyOutcome::Merged { path, added_lines }) => {
                if added_lines == 0 {
                    merged_no_op += 1;
                    detail_lines.push(format!(
                        "{} — already up to date",
                        path.display()
                    ));
                } else {
                    merged_with_additions += 1;
                    detail_lines.push(format!(
                        "merged {} new line{} into {}",
                        added_lines,
                        if added_lines == 1 { "" } else { "s" },
                        path.display()
                    ));
                }
            }
            Ok(ApplyOutcome::SkippedExisting { path, reason }) => {
                detail_lines.push(format!("skipped {} — {reason}", path.display()));
                skipped.push((path, reason));
            }
            Err(err) => {
                let name = tmpl.filename.to_string();
                let msg = format!("{err:#}");
                detail_lines.push(format!("{name} failed: {msg}"));
                errors.push((name, msg));
            }
        }
    }

    let summary = format!(
        "{}: {} created, {} merged, {} skipped",
        labels.project_templates_toast_prefix,
        created,
        merged_with_additions + merged_no_op,
        skipped.len() + errors.len()
    );

    let mut detail = detail_lines.join("\n");
    if !skipped.is_empty() || !errors.is_empty() {
        detail.push_str("\n\n");
        detail.push_str(labels.project_templates_toast_review_hint);
    } else {
        detail.push_str("\n\n");
        detail.push_str(labels.project_templates_toast_review_commit);
    }

    // Error severity for "nothing succeeded and at least one thing
    // failed"; otherwise a plain success toast. Mixed outcomes still
    // count as success because the failures are surfaced in the detail.
    let severity = if created + merged_with_additions + merged_no_op == 0
        && (!skipped.is_empty() || !errors.is_empty())
    {
        crate::ui::notifications::NotifSeverity::Warning
    } else {
        crate::ui::notifications::NotifSeverity::Success
    };
    app.notifications
        .push_with_detail(severity, summary.clone(), Some(detail));
    app.hud = Some(crate::app::Hud::new(summary, 1800));
}

fn current_language(app: &MergeFoxApp) -> UiLanguage {
    app.settings_modal
        .as_ref()
        .map(|m| m.language.resolved())
        .unwrap_or_else(|| app.config.ui_language.resolved())
}

struct Labels {
    heading: &'static str,
    no_repo_open: &'static str,
    current_repo: &'static str,
    default_remote: &'static str,
    auto_remote: &'static str,
    pull_strategy: &'static str,
    repo_settings_hint: &'static str,
    remote_urls: &'static str,
    no_remotes: &'static str,
    fetch_url: &'static str,
    push_url: &'static str,
    push_url_placeholder: &'static str,
    save_remote: &'static str,
    delete_remote: &'static str,
    default_badge: &'static str,
    add_remote: &'static str,
    remote_name: &'static str,
    add_remote_button: &'static str,
    provider_account: &'static str,
    provider_account_hint: &'static str,
    auto_account: &'static str,
    saved_prefs: &'static str,
    updated_remote: &'static str,
    deleted_remote: &'static str,
    added_remote: &'static str,
    rename_to: &'static str,
    rename_remote: &'static str,
    renamed_remote: &'static str,
    worktrees: &'static str,
    no_worktrees: &'static str,
    main_badge: &'static str,
    locked_badge: &'static str,
    prunable_badge: &'static str,
    remove_worktree: &'static str,
    remove_worktree_force: &'static str,
    removed_worktree: &'static str,
    loading: &'static str,
    maintenance: &'static str,
    repack_recommended: &'static str,
    run_fsck: &'static str,
    fsck_hint: &'static str,
    fsck_done: &'static str,
    fsck_clean: &'static str,
    fsck_failed: &'static str,
    run_gc: &'static str,
    gc_hint: &'static str,
    gc_done: &'static str,
    gc_failed: &'static str,
    run_gc_aggressive: &'static str,
    gc_aggressive_hint: &'static str,
    gc_aggressive_done: &'static str,
    run_repack: &'static str,
    repack_hint: &'static str,
    repack_done: &'static str,
    repack_failed: &'static str,
    sparse_checkout: &'static str,
    sparse_intro: &'static str,
    sparse_patterns_label: &'static str,
    sparse_classic_mode: &'static str,
    sparse_classic_hint: &'static str,
    sparse_enable: &'static str,
    sparse_apply: &'static str,
    sparse_apply_hint: &'static str,
    sparse_disable: &'static str,
    sparse_disable_hint: &'static str,
    sparse_applied: &'static str,
    sparse_apply_failed: &'static str,
    sparse_disabled: &'static str,
    sparse_disable_failed: &'static str,
    submodules: &'static str,
    no_submodules: &'static str,
    submodule_in_sync: &'static str,
    submodule_not_init: &'static str,
    submodule_modified: &'static str,
    submodule_conflict: &'static str,
    submodule_update: &'static str,
    submodule_sync: &'static str,
    submodule_update_all: &'static str,
    submodule_sync_all: &'static str,
    submodule_updated: &'static str,
    submodule_updated_all: &'static str,
    submodule_update_failed: &'static str,
    submodule_synced: &'static str,
    submodule_synced_all: &'static str,
    submodule_sync_failed: &'static str,
    project_templates: &'static str,
    project_templates_detected: &'static str,
    project_templates_blurb: &'static str,
    project_templates_review: &'static str,
    project_templates_collapse: &'static str,
    project_templates_apply: &'static str,
    project_templates_apply_hint: &'static str,
    project_templates_cancel: &'static str,
    project_templates_toast_prefix: &'static str,
    project_templates_toast_review_commit: &'static str,
    project_templates_toast_review_hint: &'static str,
}

fn labels(lang: UiLanguage) -> Labels {
    match lang {
        UiLanguage::Korean => Labels {
            heading: "저장소 설정",
            no_repo_open:
                "저장소를 열면 기본 원격, Pull 전략, 원격 URL 편집 같은 저장소별 설정을 관리할 수 있습니다.",
            current_repo: "대상 저장소:",
            default_remote: "기본 원격 저장소",
            auto_remote: "(자동 선택)",
            pull_strategy: "기본 Pull 전략",
            repo_settings_hint:
                "여기서 저장한 기본 원격과 Pull 전략은 fetch/pull/push 동작의 기본값으로 사용됩니다.",
            remote_urls: "원격 저장소 URL",
            no_remotes: "등록된 원격이 없습니다.\n아래 \"원격 저장소 추가\" 폼에서 fetch/push URL을 넣거나, 웰컴 화면에서 Publish 플로우를 사용하세요.",
            fetch_url: "Fetch URL",
            push_url: "Push URL",
            push_url_placeholder: "비워두면 Fetch URL을 사용합니다",
            save_remote: "URL 저장",
            delete_remote: "삭제",
            default_badge: "기본값",
            add_remote: "원격 저장소 추가",
            remote_name: "원격 이름",
            add_remote_button: "추가",
            provider_account: "Push/Pull 계정",
            provider_account_hint: "이 저장소에서 push/pull/fetch 시 사용할 계정입니다. HTTPS는 저장된 토큰을, SSH remote는 그 계정에 바인딩된 SSH 키를 사용합니다.",
            auto_account: "(원격 URL에서 자동 감지)",
            saved_prefs: "저장소 기본값을 저장했습니다",
            updated_remote: "원격 URL을 업데이트했습니다",
            deleted_remote: "원격을 삭제했습니다",
            added_remote: "원격을 추가했습니다",
            rename_to: "새 이름",
            rename_remote: "이름 변경",
            renamed_remote: "원격 이름을 변경했습니다",
            worktrees: "Worktrees",
            no_worktrees: "No linked worktrees.\nWorktrees let you have multiple branches checked out at once from the same repo. Add one from a terminal: `git worktree add <path> <branch>`.",
            main_badge: "main",
            locked_badge: "locked",
            prunable_badge: "prunable",
            remove_worktree: "Remove",
            remove_worktree_force: "Force remove",
            removed_worktree: "Removed worktree",
            loading: "불러오는 중…",
            maintenance: "유지보수",
            repack_recommended: "⚠ 느슨한 객체가 많습니다 — repack을 권장합니다.",
            run_fsck: "fsck 실행",
            fsck_hint: "객체 데이터베이스 무결성을 검사합니다. 느림 (수 초 ~ 수 분).",
            fsck_done: "fsck 완료",
            fsck_clean: "손상 / 누락 / 매달린 객체가 감지되지 않았습니다.",
            fsck_failed: "fsck 실패",
            run_gc: "gc 실행",
            gc_hint: "느슨한 객체를 팩으로 묶고 도달 불가능한 객체를 정리합니다.",
            gc_done: "gc 완료",
            gc_failed: "gc 실패",
            run_gc_aggressive: "gc --aggressive",
            gc_aggressive_hint: "더 강한 압축 (훨씬 오래 걸림). 큰 히스토리 재작성 후 권장.",
            gc_aggressive_done: "gc --aggressive 완료",
            run_repack: "repack",
            repack_hint: "모든 팩을 하나로 통합하고 중복 팩을 제거합니다 (git repack -Ad).",
            repack_done: "repack 완료",
            repack_failed: "repack 실패",
            sparse_checkout: "Sparse checkout",
            sparse_intro: "모노레포에서 실제로 작업하는 디렉토리만 워킹트리에 체크아웃합니다. 한 줄에 하나씩, 저장소 루트 기준 디렉토리 경로를 입력하세요.",
            sparse_patterns_label: "디렉토리 패턴 (cone 모드)",
            sparse_classic_mode: "⚠ classic (non-cone) 모드가 활성화되어 있습니다 — 읽기 전용.",
            sparse_classic_hint: "이 모드는 gitignore 스타일 패턴을 사용합니다. 편집하려면 먼저 터미널에서 `git sparse-checkout reapply --cone`으로 cone 모드로 전환하세요.",
            sparse_enable: "Sparse checkout 활성화",
            sparse_apply: "패턴 적용",
            sparse_apply_hint: "현재 목록을 sparse-checkout에 적용하고 워킹트리를 재구성합니다.",
            sparse_disable: "비활성화",
            sparse_disable_hint: "Sparse checkout을 해제하고 모든 추적 파일을 워킹트리에 복원합니다.",
            sparse_applied: "Sparse checkout 패턴을 적용했습니다",
            sparse_apply_failed: "Sparse checkout 적용 실패",
            sparse_disabled: "Sparse checkout을 비활성화했습니다",
            sparse_disable_failed: "Sparse checkout 비활성화 실패",
            submodules: "서브모듈",
            no_submodules: "이 저장소에는 서브모듈이 없습니다.",
            submodule_in_sync: "동기화됨",
            submodule_not_init: "초기화 안 됨",
            submodule_modified: "변경됨",
            submodule_conflict: "충돌",
            submodule_update: "Update",
            submodule_sync: "Sync URL",
            submodule_update_all: "전체 Update",
            submodule_sync_all: "전체 Sync URL",
            submodule_updated: "서브모듈 업데이트 완료",
            submodule_updated_all: "모든 서브모듈을 업데이트했습니다",
            submodule_update_failed: "서브모듈 업데이트 실패",
            submodule_synced: "서브모듈 URL 동기화 완료",
            submodule_synced_all: "모든 서브모듈 URL을 동기화했습니다",
            submodule_sync_failed: "서브모듈 URL 동기화 실패",
            project_templates: "프로젝트 템플릿",
            project_templates_detected: "감지된 엔진:",
            project_templates_blurb: "이 저장소에 맞는 .gitattributes / .gitignore 기본값을 적용합니다. 커밋은 직접 확인한 뒤 진행하세요.",
            project_templates_review: "템플릿 검토 및 적용",
            project_templates_collapse: "접기",
            project_templates_apply: "선택한 템플릿 적용",
            project_templates_apply_hint: "체크된 파일만 작성/병합합니다. git add는 실행하지 않습니다.",
            project_templates_cancel: "취소",
            project_templates_toast_prefix: "프로젝트 템플릿",
            project_templates_toast_review_commit: "변경 사항을 워킹 트리에서 검토한 뒤 천천히 커밋하세요.",
            project_templates_toast_review_hint: "건너뛴 파일이 있습니다 — 아래 경로를 수동으로 확인하세요.",
        },
        _ => Labels {
            heading: "Repository Settings",
            no_repo_open:
                "Open a repository to manage per-repo settings like the default remote, pull strategy, and remote URLs.",
            current_repo: "Repository:",
            default_remote: "Default remote",
            auto_remote: "(auto-select)",
            pull_strategy: "Default pull strategy",
            repo_settings_hint:
                "The saved default remote and pull strategy are used as the default for fetch, pull, and push actions.",
            remote_urls: "Remote URLs",
            no_remotes: "No remotes are configured for this repository yet.\nAdd one in the \"Add Remote\" form below, or publish your current branch from the welcome screen.",
            fetch_url: "Fetch URL",
            push_url: "Push URL",
            push_url_placeholder: "Leave empty to use the fetch URL",
            save_remote: "Save URLs",
            delete_remote: "Delete",
            default_badge: "default",
            add_remote: "Add Remote",
            remote_name: "Remote name",
            add_remote_button: "Add",
            provider_account: "Push/Pull account",
            provider_account_hint: "Which connected account to use for push/pull/fetch on this repo. HTTPS uses that account's stored token; SSH remotes use the SSH key bound to that account.",
            auto_account: "(auto-detect from remote URL)",
            saved_prefs: "Saved repository defaults",
            updated_remote: "Updated remote URLs",
            deleted_remote: "Deleted remote",
            added_remote: "Added remote",
            rename_to: "Rename to",
            rename_remote: "Rename",
            renamed_remote: "Renamed remote",
            worktrees: "Worktrees",
            no_worktrees: "No linked worktrees.\nWorktrees let you have multiple branches checked out at once from the same repo. Add one from a terminal: `git worktree add <path> <branch>`.",
            main_badge: "main",
            locked_badge: "locked",
            prunable_badge: "prunable",
            remove_worktree: "Remove",
            remove_worktree_force: "Force remove",
            removed_worktree: "Removed worktree",
            loading: "Loading…",
            maintenance: "Maintenance",
            repack_recommended: "⚠ Many loose objects — repack recommended.",
            run_fsck: "Run fsck",
            fsck_hint: "Check the object database for corruption. Slow (seconds to minutes).",
            fsck_done: "fsck complete",
            fsck_clean: "No corrupt / missing / dangling objects detected.",
            fsck_failed: "fsck failed",
            run_gc: "Run gc",
            gc_hint: "Pack loose objects and prune unreachable ones.",
            gc_done: "gc complete",
            gc_failed: "gc failed",
            run_gc_aggressive: "gc --aggressive",
            gc_aggressive_hint: "Stronger compression (much slower). Useful after a big history rewrite.",
            gc_aggressive_done: "gc --aggressive complete",
            run_repack: "Repack",
            repack_hint: "Consolidate every pack into one and delete redundant packs (git repack -Ad).",
            repack_done: "repack complete",
            repack_failed: "repack failed",
            sparse_checkout: "Sparse checkout",
            sparse_intro: "Check out only the directories you actively work in — useful for monorepos. One directory path per line, relative to the repo root.",
            sparse_patterns_label: "Directory patterns (cone mode)",
            sparse_classic_mode: "⚠ Classic (non-cone) mode is active — read only.",
            sparse_classic_hint: "This mode uses gitignore-style patterns. To edit from here, run `git sparse-checkout reapply --cone` in a terminal first.",
            sparse_enable: "Enable sparse checkout",
            sparse_apply: "Apply patterns",
            sparse_apply_hint: "Apply the current list and reconfigure the working tree.",
            sparse_disable: "Disable",
            sparse_disable_hint: "Disable sparse checkout and restore every tracked file to the working tree.",
            sparse_applied: "Sparse checkout patterns applied",
            sparse_apply_failed: "Sparse checkout apply failed",
            sparse_disabled: "Sparse checkout disabled",
            sparse_disable_failed: "Sparse checkout disable failed",
            submodules: "Submodules",
            no_submodules: "This repository has no submodules.",
            submodule_in_sync: "in sync",
            submodule_not_init: "not initialised",
            submodule_modified: "modified",
            submodule_conflict: "conflict",
            submodule_update: "Update",
            submodule_sync: "Sync URL",
            submodule_update_all: "Update all",
            submodule_sync_all: "Sync all URLs",
            submodule_updated: "Submodule updated",
            submodule_updated_all: "Updated every submodule",
            submodule_update_failed: "Submodule update failed",
            submodule_synced: "Submodule URL synced",
            submodule_synced_all: "Synced every submodule URL",
            submodule_sync_failed: "Submodule URL sync failed",
            project_templates: "Project templates",
            project_templates_detected: "Detected engine:",
            project_templates_blurb: "Drop in a curated .gitattributes / .gitignore tuned for this engine. Review the working-tree diff before committing.",
            project_templates_review: "Review & apply templates",
            project_templates_collapse: "Collapse",
            project_templates_apply: "Apply selected",
            project_templates_apply_hint: "Writes or merges the checked files. Never runs git add.",
            project_templates_cancel: "Cancel",
            project_templates_toast_prefix: "Project templates",
            project_templates_toast_review_commit: "Review and commit the changes at your own pace.",
            project_templates_toast_review_hint: "Some files were skipped — open the paths above to merge them manually.",
        },
    }
}

/// Render the "Workspace profile" block — dropdown + detection
/// diagnostic + "Switch to Game-dev mode?" banner when appropriate.
/// Writes go straight to `config.repo_settings[path].profile_override`
/// and mutate the live workspace so the change takes effect next
/// frame; no settings-modal mirroring field needed.
fn render_workspace_profile_section(ui: &mut egui::Ui, app: &mut MergeFoxApp) {
    use crate::app::View;
    let (repo_path, detected) = {
        let View::Workspace(tabs) = &app.view else {
            return;
        };
        if tabs.launcher_active {
            return;
        }
        let ws = tabs.current();
        (ws.repo.path().to_path_buf(), ws.detected_project_kind)
    };

    let repo_key = repo_path.to_string_lossy().to_string();
    let current_override = app
        .config
        .repo_settings
        .get(&repo_key)
        .and_then(|s| s.profile_override);
    let team_profile =
        crate::workspace_profile::load_team_profile(&repo_path).unwrap_or_default();
    let effective = crate::workspace_profile::effective_profile(&team_profile, current_override);

    ui.add_space(10.0);
    ui.label(RichText::new("Workspace profile").strong());

    // Dropdown: reads effective, writes user override.
    let mut selection = current_override.unwrap_or(effective);
    let before = selection;
    ComboBox::from_id_salt("settings_workspace_profile")
        .selected_text(selection.label())
        .show_ui(ui, |ui| {
            ui.selectable_value(
                &mut selection,
                WorkspaceProfile::General,
                WorkspaceProfile::General.label(),
            );
            ui.selectable_value(
                &mut selection,
                WorkspaceProfile::GameDev,
                WorkspaceProfile::GameDev.label(),
            );
            // Minimal is reserved — render disabled so users can see
            // it's on the roadmap without committing to its behaviour.
            ui.add_enabled_ui(false, |ui| {
                ui.selectable_value(
                    &mut selection,
                    WorkspaceProfile::Minimal,
                    format!("{} (coming soon)", WorkspaceProfile::Minimal.label()),
                );
            });
        });
    if selection != before {
        set_profile_override(app, &repo_path, Some(selection));
    }

    ui.weak(selection.description());

    // Team default diagnostic — educates users when the override comes
    // from a committed `.mergefox/workspace.toml`.
    if let Some(team) = team_profile.profile {
        ui.weak(format!(
            "Team default (from .mergefox/workspace.toml): {}",
            team.label()
        ));
    }

    // Detection diagnostic.
    match detected {
        Some(kind) => {
            ui.weak(format!("Detected: {}", kind.label()));
        }
        None => {
            ui.weak("No game-engine markers found.");
        }
    }

    // Suggestion banner: detected game engine + still on General + no
    // explicit override. Clicking [Switch] writes the override + saves.
    let show_suggest = detected.is_some()
        && current_override.is_none()
        && team_profile.profile.is_none()
        && effective == WorkspaceProfile::General;
    if show_suggest {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("⚡").color(Color32::from_rgb(240, 180, 96)));
            ui.label("This looks like a game-engine project. Switch to Game-dev mode?");
            if ui.button("Switch").clicked() {
                set_profile_override(app, &repo_path, Some(WorkspaceProfile::GameDev));
            }
        });
    }
}

fn set_profile_override(
    app: &mut MergeFoxApp,
    repo_path: &std::path::Path,
    override_to: Option<WorkspaceProfile>,
) {
    let key = repo_path.to_string_lossy().to_string();
    let mut settings = app
        .config
        .repo_settings
        .get(&key)
        .cloned()
        .unwrap_or_default();
    settings.profile_override = override_to;
    app.config.set_repo_settings(repo_path, settings);
    let _ = app.config.save();

    // Refresh the live workspace's cached profile so subsequent frames
    // use the new value.
    if let crate::app::View::Workspace(tabs) = &mut app.view {
        if !tabs.launcher_active {
            let ws = tabs.current_mut();
            if ws.repo.path() == repo_path {
                let team = crate::workspace_profile::load_team_profile(repo_path)
                    .unwrap_or_default();
                ws.workspace_profile =
                    crate::workspace_profile::effective_profile(&team, override_to);
            }
        }
    }
    app.notify_ok("Workspace profile updated.");
}

/// Render the "Open-with DCC applications" editor — per-extension
/// command templates with `{file}` substitution. Values are written
/// through directly to `app.config.dcc_apps.mappings`; `Config::save`
/// is called once per change rather than batched because the delta
/// is tiny and the file-system write is cheap.
fn render_dcc_apps_section(ui: &mut egui::Ui, app: &mut MergeFoxApp) {
    use crate::app::View;
    ui.add_space(12.0);
    ui.label(RichText::new("Open-with DCC applications").strong());
    ui.weak(
        "Configure per-extension launch commands. Leave blank to fall back to the system \
         default. Use `{file}` in the template to control argument placement.",
    );

    // Collect the current mappings and render them as editable rows.
    // We clone the keys into a Vec so we can mutate the map while
    // iterating — BTreeMap doesn't allow that otherwise.
    let keys: Vec<String> = app.config.dcc_apps.mappings.keys().cloned().collect();
    let mut dirty = false;
    let mut to_delete: Option<String> = None;

    egui::Grid::new("dcc_apps_grid")
        .num_columns(3)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.label(RichText::new("Extension").weak());
            ui.label(RichText::new("Command template").weak());
            ui.label("");
            ui.end_row();

            for ext in &keys {
                ui.label(format!(".{ext}"));
                let current = app
                    .config
                    .dcc_apps
                    .mappings
                    .get(ext)
                    .cloned()
                    .unwrap_or_default();
                let mut edited = current.clone();
                let resp = ui.add(
                    TextEdit::singleline(&mut edited)
                        .desired_width(f32::INFINITY)
                        .hint_text("/path/to/app {file}"),
                );
                if resp.lost_focus() && edited != current {
                    app.config
                        .dcc_apps
                        .mappings
                        .insert(ext.clone(), edited);
                    dirty = true;
                }
                if ui.small_button("×").on_hover_text("Remove mapping").clicked() {
                    to_delete = Some(ext.clone());
                }
                ui.end_row();
            }
        });

    if let Some(ext) = to_delete {
        app.config.dcc_apps.mappings.remove(&ext);
        dirty = true;
    }

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui.button("Add mapping").clicked() {
            // Seed a blank "" key so the user can type the extension
            // in the row. We insert "" because BTreeMap needs a unique
            // key; the editor row treats empty extension as
            // placeholder. To keep this simple, just auto-append a
            // numeric placeholder if "" already exists.
            let mut key = String::new();
            if app.config.dcc_apps.mappings.contains_key(&key) {
                let mut n = 1;
                loop {
                    key = format!("new{n}");
                    if !app.config.dcc_apps.mappings.contains_key(&key) {
                        break;
                    }
                    n += 1;
                }
            }
            app.config.dcc_apps.mappings.insert(key, String::new());
            dirty = true;
        }

        // "Suggest defaults" button — populates blank entries for the
        // current project kind. Idempotent: never overwrites an
        // existing value.
        let detected = match &app.view {
            View::Workspace(tabs) if !tabs.launcher_active => tabs.current().detected_project_kind,
            _ => None,
        };
        ui.add_enabled_ui(detected.is_some(), |ui| {
            let label = match detected {
                Some(kind) => format!("Suggest defaults for {}", kind.label()),
                None => "Suggest defaults".to_string(),
            };
            if ui.button(label).clicked() {
                if let Some(kind) = detected {
                    for ext in crate::ui::open_with::suggested_mappings_for_kind(kind) {
                        app.config
                            .dcc_apps
                            .mappings
                            .entry(ext.to_string())
                            .or_insert_with(String::new);
                    }
                    dirty = true;
                }
            }
        });
    });

    if dirty {
        let _ = app.config.save();
    }
}

