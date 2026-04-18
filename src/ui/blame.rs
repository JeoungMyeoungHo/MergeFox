//! Blame modal — shows per-line authorship for a file.
//!
//! Opened from the diff view's file toolbar ("Blame this file") when
//! a file is selected. The modal spawns a background thread to run
//! `git blame --porcelain` and polls the result each frame. The panel
//! lists `<short-sha> <author> <date>  <content>` rows, with distinct
//! accent colours per author so runs by the same person are visually
//! grouped.
//!
//! Not wired into the diff view header yet — this ships the viewer
//! + data plumbing; the entry point (a "Blame" button next to a file
//! name) is added incrementally as `TODO/production.md` §E7 evolves.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Instant;

use egui::{Align2, Color32, RichText};

use crate::app::{MergeFoxApp, View};
use crate::git::{BlameLine, BlameResult};

/// In-flight blame task. `None` = not running. Polled from `update`
/// each frame; holds the channel until the worker sends a result.
pub struct BlameTask {
    pub path: PathBuf,
    pub started_at: Instant,
    rx: Receiver<Result<BlameResult, String>>,
}

impl BlameTask {
    pub fn spawn(repo_path: PathBuf, file: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let file_clone = file.clone();
        thread::spawn(move || {
            let result =
                crate::git::blame_file(&repo_path, &file_clone).map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        Self {
            path: file,
            started_at: Instant::now(),
            rx,
        }
    }

    pub fn poll(&self) -> Option<Result<BlameResult, String>> {
        self.rx.try_recv().ok()
    }
}

/// Modal state — lives on `MergeFoxApp::blame`.
#[derive(Default)]
pub struct BlameState {
    pub open: bool,
    pub task: Option<BlameTask>,
    pub result: Option<BlameResult>,
    pub error: Option<String>,
}

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    // Poll the running task every frame even when the window is
    // closed — if the user closed mid-run, we still want to drain the
    // channel so the thread can exit cleanly.
    if let Some(task) = app.blame.task.as_ref() {
        if let Some(res) = task.poll() {
            app.blame.task = None;
            match res {
                Ok(result) => {
                    app.blame.result = Some(result);
                    app.blame.error = None;
                }
                Err(err) => {
                    app.blame.error = Some(err);
                    app.blame.result = None;
                }
            }
        }
    }

    if !app.blame.open {
        return;
    }

    let mut close = false;
    egui::Window::new("Blame")
        .collapsible(false)
        .resizable(true)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .default_size([720.0, 520.0])
        .min_width(480.0)
        .min_height(320.0)
        .show(ctx, |ui| {
            if app.blame.task.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(
                        RichText::new(format!(
                            "Computing blame… ({:.1}s)",
                            app.blame
                                .task
                                .as_ref()
                                .map(|t| t.started_at.elapsed().as_secs_f32())
                                .unwrap_or(0.0)
                        ))
                        .weak(),
                    );
                });
                return;
            }
            if let Some(err) = app.blame.error.as_ref() {
                ui.colored_label(Color32::LIGHT_RED, format!("blame failed: {err}"));
                ui.add_space(6.0);
                if ui.button("Close").clicked() {
                    close = true;
                }
                return;
            }
            let Some(result) = app.blame.result.clone() else {
                ui.weak("No blame loaded.");
                if ui.button("Close").clicked() {
                    close = true;
                }
                return;
            };

            ui.label(
                RichText::new(result.path.display().to_string())
                    .monospace()
                    .strong(),
            );
            ui.weak(format!("{} lines", result.lines.len()));
            ui.separator();

            // Stable colour per author email, so runs by the same
            // committer are visually grouped. Hash-derived palette
            // keeps the UI deterministic across sessions.
            let mut author_colors: HashMap<String, Color32> = HashMap::new();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for line in &result.lines {
                        let color = *author_colors
                            .entry(line.commit.author_email.clone())
                            .or_insert_with(|| author_color(&line.commit.author_email));
                        render_line(ui, line, color);
                    }
                });

            ui.add_space(4.0);
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Close").clicked() {
                    close = true;
                }
                if ui.button("Copy all").clicked() {
                    let text = result
                        .lines
                        .iter()
                        .map(|l| {
                            format!(
                                "{}\t{}\t{}\t{}\t{}",
                                short_sha(&l.commit.sha),
                                l.commit.author,
                                format_ts(l.commit.author_time),
                                l.line_no,
                                l.content
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    ctx.copy_text(text);
                }
            });
        });

    if close {
        app.blame.open = false;
    }
    // Close on Escape when no other modal above us wants it.
    if !ctx.wants_keyboard_input()
        && ctx.input(|i| i.key_pressed(egui::Key::Escape))
        && app.blame.open
    {
        app.blame.open = false;
    }
}

fn render_line(ui: &mut egui::Ui, line: &BlameLine, author_color: Color32) {
    ui.horizontal(|ui| {
        ui.monospace(RichText::new(short_sha(&line.commit.sha)).color(Color32::from_gray(140)));
        ui.add_sized(
            [110.0, 14.0],
            egui::Label::new(
                RichText::new(truncate(&line.commit.author, 18))
                    .color(author_color)
                    .small(),
            )
            .truncate(),
        );
        ui.add_sized(
            [70.0, 14.0],
            egui::Label::new(
                RichText::new(format_ts(line.commit.author_time))
                    .color(Color32::from_gray(120))
                    .small(),
            ),
        );
        ui.monospace(RichText::new(format!("{:>5}", line.line_no)).color(Color32::from_gray(120)));
        ui.monospace(RichText::new(&line.content));
    })
    .response
    .on_hover_text(format!(
        "{}\n{}\n<{}>",
        line.commit.summary, line.commit.author, line.commit.author_email
    ));
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(8).collect()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn format_ts(unix: i64) -> String {
    // Format as YY-MM-DD without a chrono dep: divmod the Unix epoch
    // approximately. Precise enough for a blame column; users looking
    // for hour-level detail should open the commit.
    if unix <= 0 {
        return "          ".to_string();
    }
    // Delegate to the system via `chrono::NaiveDateTime`? We don't
    // have chrono. Keep a tiny civil-time computation inline.
    let days = unix / 86_400;
    let (year, month, day) = civil_from_days(days);
    format!("{:04}-{:02}-{:02}", year, month, day)
}

/// Days since Unix epoch → civil (year, month, day). Adapted from
/// Howard Hinnant's public-domain date algorithm; good for years
/// 1970…∞ without depending on chrono.
fn civil_from_days(mut z: i64) -> (i32, u32, u32) {
    z += 719468;
    let era = if z >= 0 {
        z / 146097
    } else {
        (z - 146096) / 146097
    };
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y } as i32;
    (year, m, d)
}

/// Deterministic pastel colour from an arbitrary string.
fn author_color(key: &str) -> Color32 {
    let h = fnv1a(key.as_bytes());
    let hue = (h % 360) as f32;
    hsl_to_color(hue, 0.55, 0.72)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn hsl_to_color(h: f32, s: f32, l: f32) -> Color32 {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h_prime = h / 60.0;
    let x = c * (1.0 - (h_prime % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match h_prime as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0) as u8;
    Color32::from_rgb(to(r1), to(g1), to(b1))
}
