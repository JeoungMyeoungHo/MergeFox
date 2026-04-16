//! Generic text-input + confirmation modal used by context menu actions
//! that need user input (branch name, tag message, …) or a destructive-op
//! confirmation (delete branch, hard reset, force push).
//!
//! We keep one `PendingPrompt` on the app and render at most one modal at
//! a time; the modal owns its own buffer (`input`) so the user can edit
//! mid-flight without the action struct knowing about text state.

use gix::ObjectId as Oid;

use crate::actions::ResetMode;
use crate::app::{MergeFoxApp, View};

/// What the currently-open prompt is collecting.
#[derive(Debug, Clone)]
pub enum PendingPrompt {
    CreateBranch {
        at: Oid,
        /// Live buffer the user types into.
        name: String,
        /// True once the user pressed OK and we're just waiting to dispatch —
        /// lets us close the modal before running the git op.
        submitted: bool,
    },
    RenameBranch {
        from: String,
        to: String,
        submitted: bool,
    },
    CreateTag {
        at: Oid,
        name: String,
        message: String,
        annotated: bool,
        submitted: bool,
    },
    /// Set / change / clear the upstream tracking ref for a local branch.
    ///
    /// We gather the list of currently-configured remotes so the user
    /// can pick one from a dropdown instead of typing `origin/main` by
    /// hand. If no remotes exist, we surface an inline "add new remote"
    /// form so the happy path "first push ever" doesn't bounce the user
    /// to Settings → Remotes and back.
    SetUpstream {
        branch: String,
        /// Remotes pulled from `repo_ui_cache` at open time.
        remotes: Vec<String>,
        /// Currently-selected remote. `None` = "(remove upstream)".
        selected_remote: Option<String>,
        /// What branch on the remote to track. Defaults to the local name.
        remote_branch: String,
        /// Inline "add new remote" form state. `None` when the form is
        /// collapsed; `Some(draft)` while the user is filling it in.
        new_remote: Option<NewRemoteDraft>,
        submitted: bool,
    },
    AmendMessage {
        message: String,
        submitted: bool,
    },
    /// Create a new stash entry. `message` is optional (empty → git uses its
    /// default "WIP on <branch>: <sha> <subject>").
    StashPush {
        message: String,
        submitted: bool,
    },
    /// Confirmation-only (no text input). `confirmed` flips true on OK.
    Confirm {
        kind: ConfirmKind,
        confirmed: bool,
    },
}

/// Inline "add remote" draft embedded inside the Set-upstream prompt.
/// Same shape as `crate::app::RemoteDraft` but scoped to this flow so
/// we don't couple the two modals.
#[derive(Debug, Clone, Default)]
pub struct NewRemoteDraft {
    pub name: String,
    pub fetch_url: String,
    /// Empty = use fetch URL for both fetch and push.
    pub push_url: String,
}

#[derive(Debug, Clone)]
pub enum ConfirmKind {
    DeleteBranch { name: String, is_remote: bool },
    HardReset { branch: String, target: Oid },
    DropCommit { oid: Oid },
    DropStash { index: usize, message: String },
    ForcePush { remote: String, branch: String },
}

impl ConfirmKind {
    fn title(&self) -> &'static str {
        match self {
            Self::DeleteBranch { .. } => "Delete branch?",
            Self::HardReset { .. } => "Hard reset?",
            Self::DropCommit { .. } => "Drop commit?",
            Self::DropStash { .. } => "Drop stash?",
            Self::ForcePush { .. } => "Force push?",
        }
    }

    fn body(&self) -> String {
        match self {
            Self::DeleteBranch { name, is_remote } => format!(
                "Delete {} branch `{name}`?\n\nThis cannot be undone by mergefox\n(refs are still in the reflog, though).",
                if *is_remote { "remote-tracking" } else { "local" }
            ),
            Self::HardReset { branch, target } => format!(
                "Hard-reset branch `{branch}` to {}?\n\nAny uncommitted changes will be LOST.\nWe'll auto-stash first if the tree is dirty.",
                short_sha(target)
            ),
            Self::DropCommit { oid } => format!(
                "Drop commit {}?\n\nUses a rebase to remove this single commit.\nNot yet implemented.",
                short_sha(oid)
            ),
            Self::DropStash { index, message } => format!(
                "Drop stash@{{{index}}}?\n\n{message}\n\nThe stash will be removed from the stash list. Recoverable via reflog only for a short time."
            ),
            Self::ForcePush { remote, branch } => format!(
                "Force-push `{branch}` to `{remote}`?\n\n\
                 ⚠ This OVERWRITES the remote branch with your local version.\n\
                 Commits on the remote that aren't in your local history will be LOST.\n\n\
                 Use this after amend, rebase, or reset — never on a shared branch\n\
                 unless you've coordinated with other contributors."
            ),
        }
    }
}

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    // Early-out: no workspace, no prompt.
    if app.pending_prompt.is_none() {
        return;
    }
    if !matches!(app.view, View::Workspace(_)) {
        app.pending_prompt = None;
        return;
    }

    let mut close = false;
    let mut submitted = false;
    let title = prompt_title(app.pending_prompt.as_ref().unwrap());

    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .default_width(360.0)
        .show(ctx, |ui| {
            let Some(prompt) = app.pending_prompt.as_mut() else {
                return;
            };
            match prompt {
                PendingPrompt::CreateBranch { name, .. } => {
                    ui.label("Branch name:");
                    let resp = ui.text_edit_singleline(name);
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let ok = !name.trim().is_empty();
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                        ui.add_enabled_ui(ok, |ui| {
                            if ui.button("Create").clicked() || (enter && ok) {
                                submitted = true;
                            }
                        });
                    });
                }
                PendingPrompt::RenameBranch { from, to, .. } => {
                    ui.label(format!("Rename `{from}` to:"));
                    let resp = ui.text_edit_singleline(to);
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let ok = !to.trim().is_empty() && to != from;
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                        ui.add_enabled_ui(ok, |ui| {
                            if ui.button("Rename").clicked() || (enter && ok) {
                                submitted = true;
                            }
                        });
                    });
                }
                PendingPrompt::CreateTag {
                    name,
                    message,
                    annotated,
                    ..
                } => {
                    ui.checkbox(annotated, "Annotated (includes message + author)");
                    ui.label("Tag name:");
                    ui.text_edit_singleline(name);
                    if *annotated {
                        ui.label("Message:");
                        ui.text_edit_multiline(message);
                    }
                    let ok = !name.trim().is_empty();
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                        ui.add_enabled_ui(ok, |ui| {
                            if ui.button("Create tag").clicked() {
                                submitted = true;
                            }
                        });
                    });
                }
                PendingPrompt::SetUpstream {
                    branch,
                    remotes,
                    selected_remote,
                    remote_branch,
                    new_remote,
                    ..
                } => {
                    ui.label(format!(
                        "Track a remote branch for local branch `{branch}`."
                    ));
                    ui.add_space(4.0);

                    if remotes.is_empty() && new_remote.is_none() {
                        // Zero-remotes happy path: tell the user + offer a
                        // one-click "add a remote first".
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 170, 60),
                            "⚠ This repo has no remotes configured yet.",
                        );
                        ui.add_space(4.0);
                        if ui.button("➕ Add a new remote…").clicked() {
                            *new_remote = Some(NewRemoteDraft::default());
                        }
                    } else if !remotes.is_empty() {
                        ui.horizontal(|ui| {
                            ui.label("Remote:");
                            let current = selected_remote
                                .clone()
                                .unwrap_or_else(|| "(remove upstream)".to_string());
                            egui::ComboBox::from_id_salt("set_upstream_remote")
                                .selected_text(current)
                                .show_ui(ui, |ui| {
                                    for r in remotes.iter() {
                                        let selected = selected_remote.as_ref() == Some(r);
                                        if ui.selectable_label(selected, r).clicked() {
                                            *selected_remote = Some(r.clone());
                                        }
                                    }
                                    ui.separator();
                                    let clear_selected = selected_remote.is_none();
                                    if ui
                                        .selectable_label(clear_selected, "(remove upstream)")
                                        .clicked()
                                    {
                                        *selected_remote = None;
                                    }
                                });
                            if selected_remote.is_some()
                                && ui
                                    .small_button("➕ New")
                                    .on_hover_text("Add another remote")
                                    .clicked()
                            {
                                *new_remote = Some(NewRemoteDraft::default());
                            }
                        });

                        if selected_remote.is_some() {
                            ui.horizontal(|ui| {
                                ui.label("Remote branch:");
                                ui.text_edit_singleline(remote_branch);
                            });
                            ui.weak(format!(
                                "→ will track `{}/{}`",
                                selected_remote.as_deref().unwrap_or(""),
                                remote_branch
                            ));
                        } else {
                            ui.weak(format!("→ will clear upstream tracking for `{branch}`"));
                        }
                    }

                    // Inline "add new remote" form. Lives alongside the
                    // dropdown so you don't have to bounce through Settings.
                    if let Some(draft) = new_remote.as_mut() {
                        ui.add_space(6.0);
                        ui.separator();
                        ui.label(egui::RichText::new("Add remote").strong());
                        ui.horizontal(|ui| {
                            ui.label("Name:");
                            ui.text_edit_singleline(&mut draft.name);
                        });
                        ui.weak("Usually `origin` for the main remote.");
                        ui.horizontal(|ui| {
                            ui.label("Fetch URL:");
                            ui.text_edit_singleline(&mut draft.fetch_url);
                        });
                        ui.horizontal(|ui| {
                            ui.label("Push URL:");
                            ui.text_edit_singleline(&mut draft.push_url);
                        });
                        ui.weak("Leave empty to use the fetch URL for both.");
                    }

                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                        let can_submit = if new_remote.is_some() {
                            // Creating a new remote: require name + fetch URL.
                            let d = new_remote.as_ref().unwrap();
                            !d.name.trim().is_empty() && !d.fetch_url.trim().is_empty()
                        } else if selected_remote.is_some() {
                            // Setting a tracking ref: require a branch name.
                            !remote_branch.trim().is_empty()
                        } else {
                            // Clearing upstream is always valid.
                            true
                        };
                        let label = if new_remote.is_some() {
                            "Add remote & set upstream"
                        } else if selected_remote.is_some() {
                            "Set upstream"
                        } else {
                            "Clear upstream"
                        };
                        ui.add_enabled_ui(can_submit, |ui| {
                            if ui.button(label).clicked() {
                                submitted = true;
                            }
                        });
                    });
                }
                PendingPrompt::AmendMessage { message, .. } => {
                    ui.label("New commit message:");
                    ui.add(
                        egui::TextEdit::multiline(message)
                            .desired_rows(4)
                            .desired_width(f32::INFINITY),
                    );
                    let ok = !message.trim().is_empty();
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                        ui.add_enabled_ui(ok, |ui| {
                            if ui.button("Amend").clicked() {
                                submitted = true;
                            }
                        });
                    });
                }
                PendingPrompt::StashPush { message, .. } => {
                    ui.label("Stash message (optional):");
                    let resp = ui.text_edit_singleline(message);
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add_space(4.0);
                    ui.weak(
                        "Saves the working tree + index (including untracked files) and resets\n\
                         the working tree to HEAD. The stash can be popped or applied later.",
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                        if ui.button("Stash").clicked() || enter {
                            submitted = true;
                        }
                    });
                }
                PendingPrompt::Confirm { kind, .. } => {
                    ui.label(kind.body());
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                        let confirm_label = match kind {
                            ConfirmKind::DeleteBranch { .. } => "Delete",
                            ConfirmKind::HardReset { .. } => "Hard reset",
                            ConfirmKind::DropCommit { .. } => "Drop",
                            ConfirmKind::DropStash { .. } => "Drop stash",
                            ConfirmKind::ForcePush { .. } => "Force push",
                        };
                        if ui
                            .button(
                                egui::RichText::new(confirm_label).color(egui::Color32::LIGHT_RED),
                            )
                            .clicked()
                        {
                            submitted = true;
                        }
                    });
                }
            }
        });

    if close {
        app.pending_prompt = None;
        return;
    }

    if submitted {
        // Flip the "submitted" flag so the dispatcher picks it up next frame
        // (or now) and closes the modal. We keep prompt state owned here.
        if let Some(p) = app.pending_prompt.as_mut() {
            mark_submitted(p);
        }
        let taken = app.pending_prompt.take();
        if let Some(p) = taken {
            super::main_panel::dispatch_prompt(app, p);
        }
    }
}

fn mark_submitted(p: &mut PendingPrompt) {
    match p {
        PendingPrompt::CreateBranch { submitted, .. }
        | PendingPrompt::RenameBranch { submitted, .. }
        | PendingPrompt::CreateTag { submitted, .. }
        | PendingPrompt::SetUpstream { submitted, .. }
        | PendingPrompt::AmendMessage { submitted, .. }
        | PendingPrompt::StashPush { submitted, .. } => {
            *submitted = true;
        }
        PendingPrompt::Confirm { confirmed, .. } => {
            *confirmed = true;
        }
    }
}

fn prompt_title(p: &PendingPrompt) -> &'static str {
    match p {
        PendingPrompt::CreateBranch { .. } => "Create branch",
        PendingPrompt::RenameBranch { .. } => "Rename branch",
        PendingPrompt::CreateTag { .. } => "Create tag",
        PendingPrompt::SetUpstream { .. } => "Set upstream",
        PendingPrompt::AmendMessage { .. } => "Amend commit message",
        PendingPrompt::StashPush { .. } => "Create stash",
        PendingPrompt::Confirm { kind, .. } => kind.title(),
    }
}

fn short_sha(oid: &Oid) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_string()
}

// Convenience helpers used by dispatch() to kick off prompts without
// typing out the struct literal every time.
pub fn create_branch_prompt(at: Oid) -> PendingPrompt {
    PendingPrompt::CreateBranch {
        at,
        name: String::new(),
        submitted: false,
    }
}

pub fn rename_branch_prompt(from: String) -> PendingPrompt {
    PendingPrompt::RenameBranch {
        to: from.clone(),
        from,
        submitted: false,
    }
}

pub fn create_tag_prompt(at: Oid, annotated: bool) -> PendingPrompt {
    PendingPrompt::CreateTag {
        at,
        name: String::new(),
        message: String::new(),
        annotated,
        submitted: false,
    }
}

pub fn set_upstream_prompt(branch: String, remotes: Vec<String>) -> PendingPrompt {
    // Default to `origin` when it exists, otherwise the first remote,
    // otherwise no selection (triggers the "add remote" empty state).
    let selected_remote = remotes
        .iter()
        .find(|r| *r == "origin")
        .or_else(|| remotes.first())
        .cloned();
    // If no remotes exist at all, auto-open the "add remote" form so the
    // modal is immediately useful rather than a dead end.
    let new_remote = if remotes.is_empty() {
        Some(NewRemoteDraft {
            name: "origin".into(),
            ..Default::default()
        })
    } else {
        None
    };
    PendingPrompt::SetUpstream {
        remote_branch: branch.clone(),
        branch,
        remotes,
        selected_remote,
        new_remote,
        submitted: false,
    }
}

pub fn amend_message_prompt(initial: String) -> PendingPrompt {
    PendingPrompt::AmendMessage {
        message: initial,
        submitted: false,
    }
}

pub fn delete_branch_confirm(name: String, is_remote: bool) -> PendingPrompt {
    PendingPrompt::Confirm {
        kind: ConfirmKind::DeleteBranch { name, is_remote },
        confirmed: false,
    }
}

pub fn hard_reset_confirm(branch: String, target: Oid) -> PendingPrompt {
    PendingPrompt::Confirm {
        kind: ConfirmKind::HardReset { branch, target },
        confirmed: false,
    }
}

#[allow(dead_code)]
pub fn drop_commit_confirm(oid: Oid) -> PendingPrompt {
    PendingPrompt::Confirm {
        kind: ConfirmKind::DropCommit { oid },
        confirmed: false,
    }
}

pub fn stash_push_prompt() -> PendingPrompt {
    PendingPrompt::StashPush {
        message: String::new(),
        submitted: false,
    }
}

pub fn drop_stash_confirm(index: usize, message: String) -> PendingPrompt {
    PendingPrompt::Confirm {
        kind: ConfirmKind::DropStash { index, message },
        confirmed: false,
    }
}

pub fn force_push_confirm(remote: String, branch: String) -> PendingPrompt {
    PendingPrompt::Confirm {
        kind: ConfirmKind::ForcePush { remote, branch },
        confirmed: false,
    }
}

// `ResetMode` is imported at the top so this module can be used to build
// reset-related prompts without the dispatcher needing an extra import.
#[allow(dead_code)]
const _RESET_MODE_MARKER: Option<ResetMode> = None;
