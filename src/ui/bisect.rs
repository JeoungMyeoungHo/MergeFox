//! Bisect UI.
//!
//! Opened from the command palette or by detecting an in-progress
//! bisect session at repo load time. Renders the current state
//! (last progress line + good/bad/skip counters + culprit when
//! concluded) and one-click Good / Bad / Skip / Reset buttons that
//! map directly onto the corresponding `Repo::bisect_*` calls.
//!
//! Bisect is intrinsically a per-repo state machine — we do not need
//! per-workspace copies here because the git CLI reads the state
//! from `.git/BISECT_*` for whichever repo we invoke it in. We just
//! target the active workspace on each button click.

use egui::{Align2, Color32, RichText};

use crate::app::{MergeFoxApp, View};
use crate::git::BisectStatus;

#[derive(Default)]
pub struct BisectUiState {
    pub open: bool,
    /// Lazy-loaded status snapshot. Refreshed after every op so the
    /// counters reflect the most recent decision.
    pub status: Option<BisectStatus>,
    /// `start` form fields. `bad` defaults to HEAD so the common
    /// "HEAD is broken, last week's main was fine" workflow is one
    /// click.
    pub start_bad: String,
    pub start_good: String,
}

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    if !app.bisect_ui.open {
        return;
    }

    let active = match &app.view {
        View::Workspace(tabs) => tabs.current().repo.bisect_active(),
        _ => false,
    };
    // Auto-refresh status on first open and after any action.
    if active && app.bisect_ui.status.is_none() {
        if let View::Workspace(tabs) = &app.view {
            app.bisect_ui.status = tabs.current().repo.bisect_status();
        }
    }
    // If user closed the session externally, drop the cached status.
    if !active {
        app.bisect_ui.status = None;
    }

    let mut close = false;
    let mut intent: Option<Intent> = None;

    egui::Window::new("Bisect")
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .default_width(480.0)
        .show(ctx, |ui| {
            if !active {
                render_start_form(ui, &mut app.bisect_ui, &mut intent);
            } else {
                render_active(ui, &app.bisect_ui, &mut intent);
            }
            ui.add_space(10.0);
            ui.separator();
            ui.horizontal(|ui| {
                if active && ui.button("Reset session").clicked() {
                    intent = Some(Intent::Reset);
                }
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        });

    if let Some(action) = intent {
        run(app, action);
    }
    if close
        || (!ctx.wants_keyboard_input()
            && ctx.input(|i| i.key_pressed(egui::Key::Escape))
            && app.bisect_ui.open)
    {
        app.bisect_ui.open = false;
    }
}

enum Intent {
    Start { bad: String, good: String },
    Bad,
    Good,
    Skip,
    Reset,
}

fn render_start_form(
    ui: &mut egui::Ui,
    state: &mut BisectUiState,
    intent: &mut Option<Intent>,
) {
    ui.label(
        RichText::new("Start a bisect")
            .strong()
            .size(14.0),
    );
    ui.weak(
        "Mark a known-broken commit as `bad` and a known-working commit as `good`. \
         Git will narrow the range by checking out the midpoint for each round.",
    );
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("Bad:");
        ui.text_edit_singleline(&mut state.start_bad);
        ui.weak("(defaults to HEAD)");
    });
    ui.horizontal(|ui| {
        ui.label("Good:");
        ui.text_edit_singleline(&mut state.start_good);
        ui.weak("(branch, tag, or SHA)");
    });
    ui.add_space(6.0);
    let good_ok = !state.start_good.trim().is_empty();
    ui.add_enabled_ui(good_ok, |ui| {
        if ui.button("Start").clicked() {
            *intent = Some(Intent::Start {
                bad: state.start_bad.trim().to_string(),
                good: state.start_good.trim().to_string(),
            });
        }
    });
    if !good_ok {
        ui.weak("Enter at least a known-good ref to start.");
    }
}

fn render_active(ui: &mut egui::Ui, state: &BisectUiState, intent: &mut Option<Intent>) {
    ui.label(RichText::new("Bisecting").strong().size(14.0));
    if let Some(status) = state.status.as_ref() {
        if status.concluded {
            ui.colored_label(
                Color32::from_rgb(235, 108, 108),
                RichText::new(format!(
                    "First bad commit: {}",
                    status
                        .conclusion_sha
                        .as_deref()
                        .unwrap_or("(unknown SHA)")
                ))
                .strong()
                .size(13.0),
            );
            ui.weak("Run `Reset session` to return to the branch you bisected from.");
            return;
        }
        if !status.last_progress.is_empty() {
            ui.label(&status.last_progress);
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.weak(format!("✓ good: {}", status.good_count));
            ui.weak(format!("✗ bad: {}", status.bad_count));
            ui.weak(format!("⇢ skip: {}", status.skip_count));
        });
    } else {
        ui.weak("Loading status…");
    }
    ui.add_space(8.0);
    ui.weak("Check the current HEAD — does the bug reproduce?");
    ui.horizontal_wrapped(|ui| {
        if ui
            .button(RichText::new("✓ Good").color(Color32::from_rgb(116, 192, 136)))
            .on_hover_text("Mark this commit as NOT reproducing the bug.")
            .clicked()
        {
            *intent = Some(Intent::Good);
        }
        if ui
            .button(RichText::new("✗ Bad").color(Color32::from_rgb(235, 108, 108)))
            .on_hover_text("Mark this commit as reproducing the bug.")
            .clicked()
        {
            *intent = Some(Intent::Bad);
        }
        if ui
            .button("Skip")
            .on_hover_text("Can't test this commit (doesn't build, etc.) — let git pick another.")
            .clicked()
        {
            *intent = Some(Intent::Skip);
        }
    });
}

fn run(app: &mut MergeFoxApp, action: Intent) {
    let result = {
        let View::Workspace(tabs) = &app.view else {
            app.notify_warn("Open a repository first.");
            return;
        };
        let repo = &tabs.current().repo;
        match action {
            Intent::Start { bad, good } => {
                let b = if bad.is_empty() { None } else { Some(bad.as_str()) };
                let g = Some(good.as_str());
                repo.bisect_start(b, g)
            }
            Intent::Good => repo.bisect_good(),
            Intent::Bad => repo.bisect_bad(),
            Intent::Skip => repo.bisect_skip(),
            Intent::Reset => repo.bisect_reset(),
        }
    };
    match result {
        Ok(output) => {
            if !output.trim().is_empty() {
                tracing::info!(target: "mergefox::bisect", output = %output);
            }
            app.bisect_ui.status = None; // force refresh
            // HEAD likely moved — rebuild the graph so the user sees it.
            if let View::Workspace(tabs) = &app.view {
                let scope = tabs.current().graph_scope;
                app.rebuild_graph(scope);
            }
        }
        Err(err) => {
            app.notify_err_with_detail("bisect failed", format!("{err:#}"));
        }
    }
}
