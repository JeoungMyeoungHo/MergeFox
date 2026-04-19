//! Notification center — a single queue for every transient message.
//!
//! Until this pass the codebase had three parallel surfaces for
//! "something happened, tell the user":
//!
//!   * `app.hud: Option<Hud>` — single slot, auto-fade
//!   * `app.last_error: Option<String>` — stuck until reassigned
//!   * ad-hoc `Feedback::Ok / Err` banners inside Settings
//!
//! They drifted: a HUD message could clear a still-live error, or a
//! Settings feedback could disappear before the user read it. This
//! module is the successor — everything funnels through
//! `MergeFoxApp::notify_*` and renders here.
//!
//! Design choices:
//!
//! * **Queue, not single slot.** Multiple notifications can be
//!   visible; newest on top. A long-running op can fire "Started X"
//!   immediately and "Finished X" later without stomping the first.
//! * **Severity-tagged lifetime.** Info / Success auto-fade; Warning
//!   and Error stick until the user clicks `×`. An error you miss is
//!   usually the one you needed to see.
//! * **Cap the queue.** Hard limit of 8 visible notifications — if a
//!   flood happens (e.g. a broken loop calling `notify_err` every
//!   frame), the oldest gets evicted instead of filling the screen.
//! * **Shared HUD surface.** The legacy `app.hud` is kept for the
//!   undo/redo "cursor + action button" specifically (it renders the
//!   journal position, which general notifications don't have). New
//!   call sites should prefer `notify_*`.

use std::collections::VecDeque;
use std::time::Instant;

use egui::{Align2, Color32, RichText, Stroke};

use crate::app::MergeFoxApp;

const MAX_VISIBLE: usize = 8;
/// All toasts are sticky: the user explicitly dismisses each via `×`.
/// Rationale: the toast is the primary surface for "something
/// happened, you probably need to know" — auto-fading even a
/// successful operation meant users who glanced away missed the
/// outcome. The × button + a hard cap on stack size is the right
/// trade-off. Still exposed per-severity so a future caller can
/// override via `NotificationCenter::push_with_duration` if needed.
const DEFAULT_DURATION: Option<u64> = None;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifSeverity {
    Info,
    Success,
    Warning,
    Error,
}

impl NotifSeverity {
    fn default_duration_ms(self) -> Option<u64> {
        DEFAULT_DURATION
    }

    fn accent(self) -> Color32 {
        match self {
            Self::Info => Color32::from_rgb(148, 170, 210),
            Self::Success => Color32::from_rgb(116, 192, 136),
            Self::Warning => Color32::from_rgb(240, 180, 96),
            Self::Error => Color32::from_rgb(235, 108, 108),
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Self::Info => "ℹ",
            Self::Success => "✓",
            Self::Warning => "⚠",
            Self::Error => "⛔",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub id: u64,
    pub message: String,
    pub severity: NotifSeverity,
    pub shown_at: Instant,
    pub duration_ms: Option<u64>,
    /// Optional details (first line shown as header, this is below as
    /// weak text). Keeps short summaries skimmable while long errors
    /// are still readable.
    pub detail: Option<String>,
    /// User has clicked close — reaped on the next render pass.
    pub dismissed: bool,
}

#[derive(Debug, Default)]
pub struct NotificationCenter {
    pub items: VecDeque<Notification>,
    next_id: u64,
}

impl NotificationCenter {
    pub fn push(&mut self, severity: NotifSeverity, message: impl Into<String>) -> u64 {
        self.push_with_detail(severity, message, None)
    }

    pub fn push_with_detail(
        &mut self,
        severity: NotifSeverity,
        message: impl Into<String>,
        detail: Option<String>,
    ) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.items.push_front(Notification {
            id,
            message: message.into(),
            severity,
            shown_at: Instant::now(),
            duration_ms: severity.default_duration_ms(),
            detail,
            dismissed: false,
        });
        // Evict old entries so a flood can't scroll the UI off-screen.
        while self.items.len() > MAX_VISIBLE {
            self.items.pop_back();
        }
        id
    }

    pub fn dismiss(&mut self, id: u64) {
        if let Some(entry) = self.items.iter_mut().find(|n| n.id == id) {
            entry.dismissed = true;
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Drop dismissed + expired entries. Call once per frame before
    /// rendering; keeps `items` bounded without allocating.
    pub fn sweep(&mut self) {
        self.items.retain(|n| {
            if n.dismissed {
                return false;
            }
            match n.duration_ms {
                Some(ms) => n.shown_at.elapsed().as_millis() as u64 <= ms + 300,
                None => true,
            }
        });
    }
}

/// Render the notification stack. Must run every frame — `sweep` culls
/// the queue before painting so expired entries vanish even on idle
/// frames.
pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    app.notifications.sweep();
    if app.notifications.items.is_empty() {
        return;
    }

    let visuals = ctx.style().visuals.clone();
    // Snapshot so we can dismiss during iteration without borrowing
    // issues.
    let items: Vec<Notification> = app.notifications.items.iter().cloned().collect();
    let mut to_dismiss: Vec<u64> = Vec::new();

    // Bottom-right stack: newest on top of the stack, growing upward.
    // The 16px margin keeps toasts clear of the window edge / status
    // bar; the negative Y leaves room for the macOS traffic-light
    // buttons equivalent on our custom window chrome.
    egui::Area::new(egui::Id::new("mergefox-notifications"))
        .order(egui::Order::Foreground)
        .anchor(Align2::RIGHT_BOTTOM, [-16.0, -16.0])
        .show(ctx, |ui| {
            ui.vertical(|ui| {
                for notif in &items {
                    let alpha = alpha_for(notif);
                    if alpha == 0 {
                        continue;
                    }
                    let bg = visuals.window_fill().gamma_multiply(alpha as f32 / 255.0);
                    let stroke = Stroke::new(
                        1.0,
                        notif.severity.accent().gamma_multiply(alpha as f32 / 255.0),
                    );
                    egui::Frame::window(ui.style())
                        .fill(bg)
                        .stroke(stroke)
                        .rounding(6.0)
                        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                        .show(ui, |ui| {
                            ui.set_max_width(360.0);
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(notif.severity.glyph())
                                        .color(notif.severity.accent())
                                        .size(14.0)
                                        .strong(),
                                );
                                ui.vertical(|ui| {
                                    ui.label(
                                        RichText::new(&notif.message)
                                            .color(
                                                visuals
                                                    .strong_text_color()
                                                    .gamma_multiply(alpha as f32 / 255.0),
                                            )
                                            .size(13.0)
                                            .strong(),
                                    );
                                    if let Some(detail) = notif.detail.as_ref() {
                                        ui.label(
                                            RichText::new(detail)
                                                .color(
                                                    visuals
                                                        .weak_text_color()
                                                        .gamma_multiply(alpha as f32 / 255.0),
                                                )
                                                .size(11.0),
                                        );
                                    }
                                });
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::TOP),
                                    |ui| {
                                        if ui.small_button("×").clicked() {
                                            to_dismiss.push(notif.id);
                                        }
                                    },
                                );
                            });
                        });
                    ui.add_space(4.0);
                }
            });
        });

    for id in to_dismiss {
        app.notifications.dismiss(id);
    }
}

/// Compute an alpha byte (0..=255) based on age. Fade-in the first
/// 150 ms, full opacity through the middle, fade-out the last 300 ms
/// when a duration is set.
fn alpha_for(n: &Notification) -> u8 {
    let age_ms = n.shown_at.elapsed().as_millis() as u64;
    let fade_in = (age_ms as f32 / 150.0).clamp(0.0, 1.0);
    let fade_out = match n.duration_ms {
        Some(ms) if age_ms + 300 >= ms => {
            ((ms.saturating_sub(age_ms)) as f32 / 300.0).clamp(0.0, 1.0)
        }
        _ => 1.0,
    };
    (fade_in * fade_out * 235.0) as u8
}

// Convenience for call sites.
impl MergeFoxApp {
    pub fn notify_ok(&mut self, msg: impl Into<String>) {
        self.notifications.push(NotifSeverity::Success, msg);
    }
    pub fn notify_info(&mut self, msg: impl Into<String>) {
        self.notifications.push(NotifSeverity::Info, msg);
    }
    pub fn notify_warn(&mut self, msg: impl Into<String>) {
        self.notifications.push(NotifSeverity::Warning, msg);
    }
    pub fn notify_err(&mut self, msg: impl Into<String>) {
        self.notifications.push(NotifSeverity::Error, msg);
    }
    pub fn notify_err_with_detail(&mut self, msg: impl Into<String>, detail: impl Into<String>) {
        self.notifications
            .push_with_detail(NotifSeverity::Error, msg, Some(detail.into()));
    }
    /// Success toast with a secondary line for long-form context (e.g.
    /// "backup tag X points at the pre-reset HEAD"). The summary is what
    /// the user sees at a glance; the detail is revealed in the toast's
    /// expanded view and never truncated.
    pub fn notify_ok_with_detail(&mut self, msg: impl Into<String>, detail: impl Into<String>) {
        self.notifications
            .push_with_detail(NotifSeverity::Success, msg, Some(detail.into()));
    }
}
