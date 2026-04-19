//! Find-and-fix modal — literal search across the working tree plus
//! commit messages, with per-hit apply toggles and a single atomic
//! "replace all checked" button.
//!
//! The modal is a read-only surface until the user clicks **Apply** —
//! scans are safe to kick off on every Search-button press and are
//! debounced from Apply, which is the only destructive step. All the
//! git work (backup tag + auto-stash envelope, working-tree rewrite,
//! batch reword) is in `git::find_fix_ops`; this file is presentation
//! + intent routing only.

use std::collections::BTreeSet;
use std::path::PathBuf;

use egui::{Color32, RichText, TextEdit};

use crate::app::MergeFoxApp;
use crate::git::find_fix_ops::{CommitMatch, WorkingTreeMatch};

/// State the modal carries between frames. Parked on `MergeFoxApp` so a
/// tab switch / notification dismissal doesn't lose what the user
/// typed + which hits they'd already ticked.
#[derive(Debug, Default)]
pub struct FindFixModalState {
    pub pattern: String,
    pub replacement: String,
    pub include_working_tree: bool,
    pub include_commit_messages: bool,
    /// Commit history depth for the scan — kept here (not hard-coded in
    /// the backend) so a huge-repo user can dial it down without
    /// rebuilding. 1000 is the default; scan completes in <1 s on a
    /// typical repo of that size.
    pub commit_history_limit: usize,
    pub working_tree_results: Vec<WorkingTreeMatch>,
    pub commit_results: Vec<CommitMatch>,
    /// Per-path tick — one file's worth of hits is the apply unit
    /// (we rewrite the whole file, not individual lines, so ticking
    /// a single line would be a lie).
    pub selected_paths: BTreeSet<PathBuf>,
    pub selected_commits: BTreeSet<gix::ObjectId>,
    pub last_scan_error: Option<String>,
    pub scan_busy: bool,
    pub apply_busy: bool,
    /// True once a scan has completed at least once — drives the
    /// "No matches" vs "Run a search" empty-state copy.
    pub scanned_at_least_once: bool,
}

impl FindFixModalState {
    pub fn new() -> Self {
        Self {
            pattern: String::new(),
            replacement: String::new(),
            include_working_tree: true,
            include_commit_messages: true,
            commit_history_limit: 1000,
            working_tree_results: Vec::new(),
            commit_results: Vec::new(),
            selected_paths: BTreeSet::new(),
            selected_commits: BTreeSet::new(),
            last_scan_error: None,
            scan_busy: false,
            apply_busy: false,
            scanned_at_least_once: false,
        }
    }
}

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    if app.find_fix_modal.is_none() {
        return;
    }

    let mut open = true;
    let mut run_scan = false;
    let mut run_apply = false;
    let mut close = false;

    egui::Window::new("Find & replace across history")
        .id(egui::Id::new("mergefox-find-fix"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(720.0)
        .default_height(520.0)
        .show(ctx, |ui| {
            let Some(state) = app.find_fix_modal.as_mut() else {
                return;
            };

            ui.label(
                RichText::new(
                    "Literal string search. Replacement is applied verbatim — no regex, \
                     no case folding. Commit-message rewrites create a backup tag and \
                     rebase descendants.",
                )
                .weak()
                .size(11.5),
            );
            ui.add_space(8.0);

            egui::Grid::new("find-fix-inputs")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Search");
                    ui.add(
                        TextEdit::singleline(&mut state.pattern)
                            .desired_width(f32::INFINITY)
                            .hint_text("e.g. Fork-style"),
                    );
                    ui.end_row();

                    ui.label("Replace with");
                    ui.add(
                        TextEdit::singleline(&mut state.replacement)
                            .desired_width(f32::INFINITY)
                            .hint_text("leave blank to delete all occurrences"),
                    );
                    ui.end_row();
                });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.checkbox(&mut state.include_working_tree, "Working tree");
                ui.checkbox(&mut state.include_commit_messages, "Commit messages");
                ui.add_space(16.0);
                ui.label(RichText::new("History depth").weak());
                let mut limit = state.commit_history_limit as u32;
                if ui
                    .add(
                        egui::DragValue::new(&mut limit)
                            .range(50..=50_000)
                            .speed(50.0),
                    )
                    .changed()
                {
                    state.commit_history_limit = limit as usize;
                }
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let can_scan = !state.pattern.is_empty()
                    && !state.scan_busy
                    && !state.apply_busy
                    && (state.include_working_tree || state.include_commit_messages);
                let scan_label = if state.scan_busy { "Scanning…" } else { "Search" };
                if ui
                    .add_enabled(can_scan, egui::Button::new(scan_label))
                    .clicked()
                {
                    run_scan = true;
                }
                if state.scan_busy {
                    ui.spinner();
                }

                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let selected_count =
                            state.selected_paths.len() + state.selected_commits.len();
                        let can_apply = selected_count > 0
                            && !state.apply_busy
                            && !state.scan_busy
                            && !state.pattern.is_empty();
                        let apply_label = if state.apply_busy {
                            "Applying…".to_string()
                        } else if selected_count == 0 {
                            "Apply".to_string()
                        } else {
                            format!("Apply ({selected_count})")
                        };
                        let resp = ui.add_enabled(
                            can_apply,
                            egui::Button::new(
                                RichText::new(apply_label)
                                    .color(Color32::WHITE)
                                    .strong(),
                            )
                            .fill(Color32::from_rgb(212, 92, 92)),
                        );
                        if resp.clicked() {
                            run_apply = true;
                        }
                        if state.apply_busy {
                            ui.add_space(4.0);
                            ui.spinner();
                        }
                    },
                );
            });

            if let Some(err) = state.last_scan_error.as_ref() {
                ui.add_space(6.0);
                ui.colored_label(Color32::from_rgb(235, 108, 108), format!("⛔ {err}"));
            }

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(6.0);

            if state.working_tree_results.is_empty() && state.commit_results.is_empty() {
                if state.scanned_at_least_once {
                    ui.weak("No matches.");
                } else {
                    ui.weak("Type a search term and press Search to scan.");
                }
            } else {
                render_results(ui, state);
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Close").clicked() {
                    close = true;
                }
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let hit_total =
                            state.working_tree_results.len() + state.commit_results.len();
                        if hit_total > 0 {
                            ui.label(
                                RichText::new(format!(
                                    "{} file hit{} · {} commit hit{}",
                                    state.working_tree_results.len(),
                                    if state.working_tree_results.len() == 1 { "" } else { "s" },
                                    state.commit_results.len(),
                                    if state.commit_results.len() == 1 { "" } else { "s" },
                                ))
                                .weak(),
                            );
                        }
                    },
                );
            });
        });

    if close || !open {
        app.find_fix_modal = None;
        return;
    }
    if run_scan {
        app.start_find_fix_scan();
    }
    if run_apply {
        app.start_find_fix_apply();
    }
}

fn render_results(ui: &mut egui::Ui, state: &mut FindFixModalState) {
    egui::ScrollArea::vertical()
        .id_salt("find-fix-results")
        .auto_shrink([false, false])
        .max_height(ui.available_height() - 48.0)
        .show(ui, |ui| {
            if !state.working_tree_results.is_empty() {
                ui.label(
                    RichText::new(format!(
                        "Working tree — {} hit{}",
                        state.working_tree_results.len(),
                        if state.working_tree_results.len() == 1 { "" } else { "s" },
                    ))
                    .strong(),
                );
                ui.add_space(2.0);

                // Group hits by path so ticking operates on a file.
                let mut grouped: std::collections::BTreeMap<PathBuf, Vec<&WorkingTreeMatch>> =
                    Default::default();
                for m in &state.working_tree_results {
                    grouped.entry(m.path.clone()).or_default().push(m);
                }
                for (path, hits) in &grouped {
                    let mut ticked = state.selected_paths.contains(path);
                    let header = format!(
                        "{} — {} hit{}",
                        path.display(),
                        hits.len(),
                        if hits.len() == 1 { "" } else { "s" }
                    );
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut ticked, "").changed() {
                            if ticked {
                                state.selected_paths.insert(path.clone());
                            } else {
                                state.selected_paths.remove(path);
                            }
                        }
                        ui.label(RichText::new(header).monospace());
                    });
                    egui::Frame::none()
                        .inner_margin(egui::Margin {
                            left: 28.0,
                            right: 4.0,
                            top: 0.0,
                            bottom: 4.0,
                        })
                        .show(ui, |ui| {
                            for hit in hits {
                                render_wt_line(ui, hit, &state.pattern);
                            }
                        });
                }
                ui.add_space(8.0);
            }

            if !state.commit_results.is_empty() {
                ui.label(
                    RichText::new(format!(
                        "Commit messages — {} hit{}",
                        state.commit_results.len(),
                        if state.commit_results.len() == 1 { "" } else { "s" },
                    ))
                    .strong(),
                );
                ui.add_space(2.0);
                let commits = state.commit_results.clone();
                for m in &commits {
                    let mut ticked = state.selected_commits.contains(&m.oid);
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut ticked, "").changed() {
                            if ticked {
                                state.selected_commits.insert(m.oid);
                            } else {
                                state.selected_commits.remove(&m.oid);
                            }
                        }
                        let short = {
                            let s = m.oid.to_string();
                            s[..7.min(s.len())].to_string()
                        };
                        ui.label(RichText::new(short).monospace().weak());
                        let tag = match (m.subject_hit, m.body_hit) {
                            (true, true) => "subject+body",
                            (true, false) => "subject",
                            (false, true) => "body",
                            _ => "",
                        };
                        if !tag.is_empty() {
                            ui.label(
                                RichText::new(format!("[{tag}]"))
                                    .monospace()
                                    .size(10.5)
                                    .weak(),
                            );
                        }
                        ui.label(&m.subject);
                    });
                }
            }
        });
}

fn render_wt_line(ui: &mut egui::Ui, hit: &WorkingTreeMatch, _pattern: &str) {
    // Format: "<lineno>   <line>" with the hit range bolded. Use a
    // horizontal layout so the line wraps as a unit.
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.label(
            RichText::new(format!("{:>5}  ", hit.line_number))
                .monospace()
                .size(11.0)
                .weak(),
        );
        let pre = &hit.line[..hit.match_start.min(hit.line.len())];
        let mid = &hit.line[hit.match_start.min(hit.line.len())..hit.match_end.min(hit.line.len())];
        let post = &hit.line[hit.match_end.min(hit.line.len())..];
        if !pre.is_empty() {
            ui.label(RichText::new(pre).monospace().size(11.0));
        }
        if !mid.is_empty() {
            ui.label(
                RichText::new(mid)
                    .monospace()
                    .size(11.0)
                    .strong()
                    .color(Color32::from_rgb(240, 180, 96)),
            );
        }
        if !post.is_empty() {
            ui.label(RichText::new(post).monospace().size(11.0));
        }
    });
}
