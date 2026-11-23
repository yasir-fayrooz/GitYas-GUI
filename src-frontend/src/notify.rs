//! WP9: toasts and the notification log (reference UI §14).
//!
//! Bottom-right stack, max 3 visible, newest on top; success/info auto-dismiss after 4 s, warnings 8 s, errors
//! stay until dismissed; hover pauses the timer; optional single action button. The ‹BELL› popover lists the last
//! 50 notifications of the session.

use std::collections::VecDeque;

use egui::{Align2, Color32, RichText, Ui};
use egui_phosphor::regular as icon;
use gityas_core::GitYasError;

use crate::app::GitYasApp;
use crate::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Success,
    Info,
    Warning,
    Error,
}

impl Level {
    fn lifetime(self) -> Option<f64> {
        match self {
            Level::Success | Level::Info => Some(4.0),
            Level::Warning => Some(8.0),
            Level::Error => None,
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Level::Success => icon::CHECK_CIRCLE,
            Level::Info => icon::INFO,
            Level::Warning => icon::WARNING,
            Level::Error => icon::WARNING_CIRCLE,
        }
    }

    fn color(self, p: &theme::Palette) -> Color32 {
        match self {
            Level::Success => p.green,
            Level::Info => p.accent,
            Level::Warning => p.orange,
            Level::Error => p.red,
        }
    }
}

/// A button on a toast. Runs once on the UI thread, then the toast is dismissed.
pub struct ToastAction {
    pub label: String,
    pub run: Box<dyn FnOnce(&mut GitYasApp)>,
}

impl ToastAction {
    pub fn new(label: impl Into<String>, run: impl FnOnce(&mut GitYasApp) + 'static) -> Self {
        Self {
            label: label.into(),
            run: Box::new(run),
        }
    }
}

pub struct Toast {
    id: u64,
    pub level: Level,
    pub title: String,
    pub detail: Option<String>,
    pub action: Option<ToastAction>,
    /// Seconds of visible (un-hovered) time left; `None` = until dismissed.
    remaining: Option<f64>,
}

/// One entry of the bell log.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub level: Level,
    pub title: String,
    pub detail: Option<String>,
    pub at: chrono::DateTime<chrono::Local>,
}

#[derive(Default)]
pub struct Notifications {
    toasts: Vec<Toast>,
    log: VecDeque<LogEntry>,
    next_id: u64,
    /// An error arrived since the bell popover was last opened (red dot on the bell).
    pub unread_errors: bool,
}

const MAX_VISIBLE: usize = 3;
const MAX_LOG: usize = 50;
const WIDTH: f32 = 380.0;

impl Notifications {
    pub fn push(&mut self, level: Level, title: impl Into<String>, detail: Option<String>, action: Option<ToastAction>) {
        let title = title.into();
        self.log.push_front(LogEntry {
            level,
            title: title.clone(),
            detail: detail.clone(),
            at: chrono::Local::now(),
        });
        self.log.truncate(MAX_LOG);
        if level == Level::Error {
            self.unread_errors = true;
        }
        self.next_id += 1;
        self.toasts.push(Toast {
            id: self.next_id,
            level,
            title,
            detail,
            action,
            remaining: level.lifetime(),
        });
    }

    pub fn success(&mut self, title: impl Into<String>) {
        self.push(Level::Success, title, None, None);
    }

    pub fn info(&mut self, title: impl Into<String>) {
        self.push(Level::Info, title, None, None);
    }

    pub fn warning(&mut self, title: impl Into<String>, detail: Option<String>) {
        self.push(Level::Warning, title, detail, None);
    }

    /// Error toast: `title` says what failed ("Could not push"), the error's message is the detail.
    pub fn error(&mut self, title: impl Into<String>, err: &GitYasError) {
        self.push(Level::Error, title, Some(err.to_string()), None);
    }

    pub fn error_with_action(&mut self, title: impl Into<String>, err: &GitYasError, action: ToastAction) {
        self.push(Level::Error, title, Some(err.to_string()), Some(action));
    }

    /// Id and title of the newest toast on screen (speed mode's collapsed bar shows new ones).
    pub fn newest(&self) -> Option<(u64, &str)> {
        self.toasts.last().map(|t| (t.id, t.title.as_str()))
    }

    #[cfg(test)]
    pub fn toasts(&self) -> impl Iterator<Item = &Toast> {
        self.toasts.iter()
    }

    #[cfg(test)]
    pub fn log(&self) -> impl Iterator<Item = &LogEntry> {
        self.log.iter()
    }

    pub fn clear_log(&mut self) {
        self.log.clear();
        self.unread_errors = false;
    }

    /// Speed mode: no toast stays longer than `timed` seconds (errors, which otherwise stay until dismissed, at most
    /// `sticky`), since the small panel has no room to keep them around.
    pub fn cap_lifetimes(&mut self, timed: f64, sticky: f64) {
        for toast in &mut self.toasts {
            let cap = if toast.level.lifetime().is_some() { timed } else { sticky };
            toast.remaining = Some(toast.remaining.map_or(cap, |left| left.min(cap)));
        }
    }

    /// Ages the visible toasts by `dt` seconds (hovered ones are paused) and drops expired ones.
    pub(crate) fn tick(&mut self, dt: f64, hovered: Option<u64>) {
        let visible: Vec<u64> = self.toasts.iter().rev().take(MAX_VISIBLE).map(|t| t.id).collect();
        for toast in &mut self.toasts {
            if Some(toast.id) == hovered || !visible.contains(&toast.id) {
                continue;
            }
            if let Some(left) = toast.remaining.as_mut() {
                *left -= dt;
            }
        }
        self.toasts.retain(|t| t.remaining.is_none_or(|left| left > 0.0));
    }

    fn dismiss(&mut self, id: u64) -> Option<Toast> {
        let pos = self.toasts.iter().position(|t| t.id == id)?;
        Some(self.toasts.remove(pos))
    }
}

/// Draws the toast stack (call last in the frame so it is on top) and runs a clicked action.
pub fn show_toasts(ui: &mut Ui, app: &mut GitYasApp) {
    let ctx = ui.ctx().clone();
    let p = theme::palette(&ctx);
    let dt = f64::from(ctx.input(|i| i.stable_dt)).min(0.1);
    let mut hovered = None;
    let mut dismissed = None;
    let mut clicked_action = None;

    let screen = ctx.content_rect();
    let mut bottom = screen.bottom() - 30.0;
    for toast in app.notify.toasts.iter().rev().take(MAX_VISIBLE) {
        let area = egui::Area::new(egui::Id::new(("toast", toast.id)))
            .order(egui::Order::Foreground)
            .pivot(Align2::RIGHT_BOTTOM)
            .fixed_pos(egui::pos2(screen.right() - 12.0, bottom))
            .show(&ctx, |ui| {
                egui::Frame::new()
                    .fill(p.bg_panel)
                    .stroke(egui::Stroke::new(1.0, p.border))
                    .corner_radius(4)
                    .inner_margin(egui::Margin {
                        left: 12,
                        right: 8,
                        top: 8,
                        bottom: 8,
                    })
                    .show(ui, |ui| {
                        ui.set_width(WIDTH - 20.0);
                        ui.horizontal_top(|ui| {
                            ui.label(RichText::new(toast.level.icon()).size(16.0).color(toast.level.color(p)));
                            ui.vertical(|ui| {
                                ui.set_width(WIDTH - 80.0);
                                ui.label(RichText::new(&toast.title).strong());
                                if let Some(detail) = &toast.detail {
                                    ui.add(egui::Label::new(RichText::new(detail).small().color(p.text_muted)).wrap());
                                }
                                if let Some(action) = &toast.action
                                    && ui.button(&action.label).clicked()
                                {
                                    clicked_action = Some(toast.id);
                                }
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                if crate::ui::icon_button(ui, icon::X, "Dismiss").clicked() {
                                    dismissed = Some(toast.id);
                                }
                            });
                        });
                    });
            });
        let rect = area.response.rect;
        // Coloured left border.
        ctx.layer_painter(area.response.layer_id).rect_filled(
            egui::Rect::from_min_max(rect.left_top(), egui::pos2(rect.left() + 3.0, rect.bottom())),
            egui::CornerRadius {
                nw: 4,
                sw: 4,
                ne: 0,
                se: 0,
            },
            toast.level.color(p),
        );
        if area.response.contains_pointer() {
            hovered = Some(toast.id);
        }
        bottom = rect.top() - 8.0;
    }

    if let Some(id) = clicked_action
        && let Some(toast) = app.notify.dismiss(id)
        && let Some(action) = toast.action
    {
        (action.run)(app);
    }
    if let Some(id) = dismissed {
        app.notify.dismiss(id);
    }
    app.notify.tick(dt, hovered);
    if app.notify.toasts.iter().any(|t| t.remaining.is_some()) {
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}

/// The status bar ‹BELL› popover body: the session's notification log.
pub fn log_popover(ui: &mut Ui, notify: &mut Notifications) {
    let p = theme::pal(ui);
    ui.set_width(360.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Notifications").strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add_enabled(!notify.log.is_empty(), egui::Button::new("Clear")).clicked() {
                notify.clear_log();
            }
        });
    });
    ui.separator();
    if notify.log.is_empty() {
        ui.label(RichText::new("No notifications").color(p.text_muted));
        return;
    }
    egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
        for entry in &notify.log {
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(entry.level.icon()).color(entry.level.color(p)));
                ui.vertical(|ui| {
                    ui.label(&entry.title);
                    if let Some(detail) = &entry.detail {
                        ui.add(egui::Label::new(RichText::new(detail).small().color(p.text_muted)).wrap());
                    }
                    ui.label(RichText::new(entry.at.format("%H:%M:%S").to_string()).small().color(p.text_faint));
                });
            });
            ui.add_space(4.0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timers_errors_and_log() {
        let mut n = Notifications::default();
        n.success("ok");
        n.error("Could not push", &GitYasError::Auth("rejected".into()));
        n.warning("careful", None);
        assert!(n.unread_errors);
        assert_eq!(n.log().count(), 3);
        n.tick(5.0, None);
        // Success expired, warning (8 s) and error (sticky) remain.
        let titles: Vec<&str> = n.toasts().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["Could not push", "careful"]);
        n.tick(5.0, None);
        assert_eq!(n.toasts().count(), 1);
        n.clear_log();
        assert!(!n.unread_errors && n.log().next().is_none());
    }

    #[test]
    fn only_visible_unhovered_toasts_age_and_log_is_capped() {
        let mut n = Notifications::default();
        for i in 0..4 {
            n.info(format!("t{i}"));
        }
        let hovered = n.toasts().last().map(|t| t.id);
        n.tick(4.5, hovered);
        // t0 is not visible (only the newest 3 are) and t3 is hovered: both survive.
        let titles: Vec<&str> = n.toasts().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["t0", "t3"]);
        for i in 0..60 {
            n.info(format!("x{i}"));
        }
        assert_eq!(n.log().count(), MAX_LOG);
    }
}
