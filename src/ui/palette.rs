//! Command palette — `Cmd/Ctrl+K`.
//!
//! A single fuzzy-searchable modal for every action the keyboard user
//! might want. We collect the candidate list each frame the modal is
//! open, filter by the current query, and render the top N with simple
//! ↑/↓/Enter navigation.
//!
//! The palette is deliberately **shallow** for now — it exposes menu
//! items (Settings, Reflog, Shortcuts, Activity log), branch checkout,
//! and the primary git network ops. Destructive / multi-step actions
//! (hard reset, rebase, drop commit) are NOT in the palette; those
//! still live behind the commit context menu where a preflight modal
//! can intercept them. See `TODO/production.md` §C1 for the long-term
//! shape.
//!
//! Fuzzy scoring is a tiny in-file matcher (no extra crate): each query
//! character must appear in order in the candidate label; score is
//! higher when matches are consecutive and earlier in the string.

use egui::{Align, Align2, Key, RichText};

use crate::app::{MergeFoxApp, View};
use crate::git::GraphScope;
use crate::ui::settings::SettingsSection;

/// Per-row height in the palette list. Constant so keyboard nav can
/// keep the focused row in view with a single `scroll_to_me` call.
const ROW_HEIGHT: f32 = 26.0;
/// Max rows the palette shows at once — keeps the modal from stretching
/// off-screen on huge repos with hundreds of branches.
const MAX_VISIBLE_RESULTS: usize = 30;

/// Concrete action a palette entry fires on Enter.
/// The palette itself is stateless; once the user picks an entry we
/// mutate `app` directly in `execute` — no intermediate message type.
#[derive(Debug, Clone)]
pub enum PaletteAction {
    OpenSettings,
    OpenSettingsSection(SettingsSection),
    OpenReflog,
    OpenShortcuts,
    OpenActivityLog,
    OpenCommitModal,
    Undo,
    Redo,
    PanicRecovery,
    CheckoutBranch(String),
    SetGraphScope(GraphScope),
    Fetch(String),
}

#[derive(Debug, Clone)]
pub struct PaletteCommand {
    pub label: String,
    pub hint: Option<String>,
    pub action: PaletteAction,
}

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    if !app.palette_open {
        return;
    }

    let commands = collect(app);
    let query = app.palette_query.clone();
    let matches = filter(&commands, &query);

    // Clamp selection in case results shrank since last frame.
    if app.palette_selected >= matches.len() {
        app.palette_selected = matches.len().saturating_sub(1);
    }

    let mut action: Option<PaletteAction> = None;
    let mut close = false;

    egui::Window::new("Command palette")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, [0.0, 80.0])
        .fixed_size([520.0, 0.0])
        .show(ctx, |ui| {
            let input = egui::TextEdit::singleline(&mut app.palette_query)
                .hint_text("Search commands, branches, actions…  (↑↓ navigate · Enter · Esc)")
                .desired_width(f32::INFINITY);
            let resp = ui.add(input);

            // Always keep focus on the input so typing works immediately
            // without a click.
            if !resp.has_focus() {
                resp.request_focus();
            }

            // If the query changed, reset the highlighted row so the
            // user doesn't stare at a stale selection from a different
            // filter result.
            if app.palette_query != query {
                app.palette_selected = 0;
            }

            ui.separator();

            if matches.is_empty() {
                ui.add_space(6.0);
                ui.weak(if app.palette_query.trim().is_empty() {
                    "Start typing to search."
                } else {
                    "No matches."
                });
                ui.add_space(6.0);
                return;
            }

            let visible = matches.len().min(MAX_VISIBLE_RESULTS);
            egui::ScrollArea::vertical()
                .max_height(ROW_HEIGHT * visible as f32 + 8.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (idx, cmd) in matches.iter().take(MAX_VISIBLE_RESULTS).enumerate() {
                        let selected = idx == app.palette_selected;
                        let row = ui.allocate_response(
                            egui::vec2(ui.available_width(), ROW_HEIGHT),
                            egui::Sense::click(),
                        );
                        let painter = ui.painter();
                        if selected {
                            painter.rect_filled(
                                row.rect,
                                4.0,
                                ui.visuals().selection.bg_fill,
                            );
                        } else if row.hovered() {
                            painter.rect_filled(
                                row.rect,
                                4.0,
                                ui.visuals().widgets.hovered.weak_bg_fill,
                            );
                        }
                        let text_color = if selected {
                            ui.visuals().selection.stroke.color
                        } else {
                            ui.visuals().text_color()
                        };
                        let label = RichText::new(&cmd.label).color(text_color);
                        painter.text(
                            row.rect.left_center() + egui::vec2(10.0, 0.0),
                            Align2::LEFT_CENTER,
                            label.text(),
                            egui::FontId::proportional(14.0),
                            text_color,
                        );
                        if let Some(hint) = cmd.hint.as_ref() {
                            painter.text(
                                row.rect.right_center() - egui::vec2(10.0, 0.0),
                                Align2::RIGHT_CENTER,
                                hint,
                                egui::FontId::proportional(12.0),
                                ui.visuals().weak_text_color(),
                            );
                        }
                        if selected {
                            row.scroll_to_me(Some(Align::Center));
                        }
                        if row.clicked() {
                            action = Some(cmd.action.clone());
                            close = true;
                        }
                    }
                });
        });

    // Keyboard handling done outside the window closure so Up/Down work
    // regardless of what has focus inside.
    let (up, down, enter, esc) = ctx.input(|i| {
        (
            i.key_pressed(Key::ArrowUp),
            i.key_pressed(Key::ArrowDown),
            i.key_pressed(Key::Enter),
            i.key_pressed(Key::Escape),
        )
    });
    if up && app.palette_selected > 0 {
        app.palette_selected -= 1;
    }
    if down && app.palette_selected + 1 < matches.len() {
        app.palette_selected += 1;
    }
    if enter && !matches.is_empty() {
        action = Some(matches[app.palette_selected].action.clone());
        close = true;
    }
    if esc {
        close = true;
    }

    if close {
        app.palette_open = false;
        app.palette_query.clear();
        app.palette_selected = 0;
    }
    if let Some(a) = action {
        execute(app, a);
    }
}

/// Collect every candidate command based on current app state. The
/// list is rebuilt every frame the palette is open — cheap because we
/// pull names from caches we already maintain for the sidebar.
fn collect(app: &MergeFoxApp) -> Vec<PaletteCommand> {
    let mut out = Vec::new();

    // Always-available menu items — same order as in the app chrome so
    // keyboard muscle-memory carries over.
    out.push(PaletteCommand {
        label: "Settings".into(),
        hint: Some("⌘,".into()),
        action: PaletteAction::OpenSettings,
    });
    out.push(PaletteCommand {
        label: "Settings → General".into(),
        hint: None,
        action: PaletteAction::OpenSettingsSection(SettingsSection::General),
    });
    out.push(PaletteCommand {
        label: "Settings → Repository".into(),
        hint: None,
        action: PaletteAction::OpenSettingsSection(SettingsSection::Repository),
    });
    out.push(PaletteCommand {
        label: "Settings → Integrations".into(),
        hint: None,
        action: PaletteAction::OpenSettingsSection(SettingsSection::Integrations),
    });
    out.push(PaletteCommand {
        label: "Settings → AI".into(),
        hint: None,
        action: PaletteAction::OpenSettingsSection(SettingsSection::Ai),
    });
    out.push(PaletteCommand {
        label: "About / Diagnostics".into(),
        hint: None,
        action: PaletteAction::OpenSettingsSection(SettingsSection::About),
    });
    out.push(PaletteCommand {
        label: "Open reflog".into(),
        hint: Some("⌘⇧R".into()),
        action: PaletteAction::OpenReflog,
    });
    out.push(PaletteCommand {
        label: "Keyboard shortcuts".into(),
        hint: Some("?".into()),
        action: PaletteAction::OpenShortcuts,
    });
    out.push(PaletteCommand {
        label: "Activity log".into(),
        hint: None,
        action: PaletteAction::OpenActivityLog,
    });
    out.push(PaletteCommand {
        label: "Undo".into(),
        hint: Some("⌘Z".into()),
        action: PaletteAction::Undo,
    });
    out.push(PaletteCommand {
        label: "Redo".into(),
        hint: Some("⌘⇧Z".into()),
        action: PaletteAction::Redo,
    });
    out.push(PaletteCommand {
        label: "Panic recovery".into(),
        hint: Some("⌘⇧Esc".into()),
        action: PaletteAction::PanicRecovery,
    });

    // Workspace-specific — only meaningful when a repo is open.
    if let View::Workspace(tabs) = &app.view {
        let ws = tabs.current();
        out.push(PaletteCommand {
            label: "New commit…".into(),
            hint: None,
            action: PaletteAction::OpenCommitModal,
        });
        out.push(PaletteCommand {
            label: "Graph scope: Current branch".into(),
            hint: None,
            action: PaletteAction::SetGraphScope(GraphScope::CurrentBranch),
        });
        out.push(PaletteCommand {
            label: "Graph scope: All local".into(),
            hint: None,
            action: PaletteAction::SetGraphScope(GraphScope::AllLocal),
        });
        out.push(PaletteCommand {
            label: "Graph scope: All refs".into(),
            hint: None,
            action: PaletteAction::SetGraphScope(GraphScope::AllRefs),
        });

        if let Some(cache) = ws.repo_ui_cache.as_ref() {
            for b in &cache.branches {
                if b.is_remote {
                    continue;
                }
                let hint = if b.is_head { Some("HEAD".into()) } else { None };
                out.push(PaletteCommand {
                    label: format!("Checkout {}", b.name),
                    hint,
                    action: PaletteAction::CheckoutBranch(b.name.clone()),
                });
            }
            for remote in &cache.remotes {
                out.push(PaletteCommand {
                    label: format!("Fetch {remote}"),
                    hint: None,
                    action: PaletteAction::Fetch(remote.clone()),
                });
            }
        }
    }

    out
}

/// Case-insensitive subsequence scorer. Returns `None` if any query
/// char is missing from the label, otherwise a higher-is-better score.
/// The scoring rewards consecutive matches and matches near the start
/// of the label — "br" beats "ba" for "branch" because both chars land
/// at positions 0..2.
fn fuzzy_score(label: &str, query: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let label_lc = label.to_lowercase();
    let mut label_chars = label_lc.chars().enumerate().peekable();
    let mut score: i32 = 0;
    let mut last_match: Option<usize> = None;
    for qc in query.to_lowercase().chars() {
        if qc == ' ' {
            continue;
        }
        let mut matched_at: Option<usize> = None;
        while let Some((idx, lc)) = label_chars.next() {
            if lc == qc {
                matched_at = Some(idx);
                break;
            }
        }
        let idx = matched_at?;
        // Consecutive bonus.
        if let Some(prev) = last_match {
            if idx == prev + 1 {
                score += 8;
            }
        }
        // Start-of-string bonus.
        if idx == 0 {
            score += 5;
        }
        // Distance penalty — later matches get diminishing scores.
        score -= idx as i32 / 4;
        last_match = Some(idx);
    }
    Some(score)
}

fn filter<'a>(commands: &'a [PaletteCommand], query: &str) -> Vec<&'a PaletteCommand> {
    let q = query.trim();
    let mut scored: Vec<(&PaletteCommand, i32)> = commands
        .iter()
        .filter_map(|c| fuzzy_score(&c.label, q).map(|s| (c, s)))
        .collect();
    // Higher score first; stable by original order for ties so the
    // canonical ordering (menu items before branches) survives an empty
    // query.
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored.into_iter().map(|(c, _)| c).collect()
}

/// Execute the chosen action against the app. Keep this pure state
/// mutation — anything fancier (prompts, jobs) should go through the
/// dispatcher or a dedicated open-modal method.
fn execute(app: &mut MergeFoxApp, action: PaletteAction) {
    match action {
        PaletteAction::OpenSettings => app.settings_open = true,
        PaletteAction::OpenSettingsSection(section) => {
            app.settings_open = true;
            if let Some(modal) = app.settings_modal.as_mut() {
                modal.section = section;
            }
        }
        PaletteAction::OpenReflog => {
            if matches!(app.view, View::Workspace(_)) {
                app.reflog_open = true;
            }
        }
        PaletteAction::OpenShortcuts => app.shortcuts_open = true,
        PaletteAction::OpenActivityLog => app.activity_log_open = true,
        PaletteAction::OpenCommitModal => {
            app.commit_modal_open = true;
        }
        PaletteAction::Undo => app.undo(),
        PaletteAction::Redo => app.redo(),
        PaletteAction::PanicRecovery => app.open_panic_recovery(),
        PaletteAction::CheckoutBranch(name) => {
            crate::ui::main_panel::dispatch_action(
                app,
                crate::actions::CommitAction::CheckoutBranch(name),
            );
        }
        PaletteAction::SetGraphScope(scope) => {
            if let View::Workspace(tabs) = &mut app.view {
                let ws = tabs.current_mut();
                ws.graph_scope = scope;
                app.rebuild_graph(scope);
            }
        }
        PaletteAction::Fetch(remote) => {
            app.start_fetch(&remote);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matches_subsequence() {
        assert!(fuzzy_score("Settings → AI", "ai").is_some());
        assert!(fuzzy_score("Open reflog", "orf").is_some());
        assert!(fuzzy_score("Checkout main", "chckm").is_some());
    }

    #[test]
    fn fuzzy_rejects_missing_chars() {
        assert!(fuzzy_score("Settings", "xyz").is_none());
    }

    #[test]
    fn consecutive_beats_scattered() {
        // "set" as a prefix of "Settings" should beat scattered "st" in
        // "Activity log" — exercises the consecutive + start-of-string
        // bonuses.
        let prefix = fuzzy_score("Settings", "set").unwrap();
        let scattered = fuzzy_score("Activity log", "act").unwrap();
        assert!(prefix >= scattered - 5, "prefix={prefix} scattered={scattered}");
    }
}
