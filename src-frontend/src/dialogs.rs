//! Modal dialog frame and the confirmation dialog.
//!
//! Every dialog in the app uses [`modal`]: title bar with `✕`, fixed width, Esc closes, Enter triggers the primary
//! button (the caller checks [`enter_pressed`]). At most one confirmation is open at a time
//! ([`GitYasApp::confirm`](crate::app::GitYasApp::confirm)).

use egui::{RichText, Ui};
use egui_phosphor::regular as icon;

use crate::app::GitYasApp;
use crate::theme;

/// Result of drawing a modal for one frame.
pub struct ModalOutcome<T> {
    pub inner: T,
    /// `✕`, Esc or a click on the backdrop: the caller should drop the dialog state.
    pub closed: bool,
}

/// Draws a modal with a title bar and `content`. `width` is the content width (400 for small dialogs, 480 for
/// add/clone, 640 for settings).
pub fn modal<T>(
    ctx: &egui::Context,
    id: impl std::hash::Hash + std::fmt::Debug,
    title: &str,
    width: f32,
    content: impl FnOnce(&mut Ui) -> T,
) -> ModalOutcome<T> {
    let p = theme::palette(ctx);
    // Never wider than the window (speed mode's panel is narrow): margins and a little room on each side.
    let width = width.min(ctx.content_rect().width() - 56.0).max(160.0);
    let mut close_clicked = false;
    let response = egui::Modal::new(egui::Id::new(id))
        .frame(
            egui::Frame::new()
                .fill(p.bg_panel)
                .stroke(egui::Stroke::new(1.0, p.border))
                .corner_radius(6)
                .inner_margin(16),
        )
        .show(ctx, |ui| {
            ui.set_width(width);
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).heading().strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    close_clicked = crate::ui::icon_button(ui, icon::X, "Close (Esc)").clicked();
                });
            });
            ui.add_space(8.0);
            content(ui)
        });
    ModalOutcome {
        closed: close_clicked || response.should_close(),
        inner: response.inner,
    }
}

/// Enter pressed this frame (and no multi-line text field has focus). Primary button shortcut.
pub fn enter_pressed(ui: &Ui) -> bool {
    ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift)
}

/// Right-aligned `[Cancel] [primary]` row. Returns `(cancel, primary)` clicks; `primary_enabled == false` greys it
/// out and shows `disabled_reason` as its tooltip.
pub fn buttons(ui: &mut Ui, primary: &str, danger: bool, primary_enabled: bool, disabled_reason: &str) -> (bool, bool) {
    let p = theme::pal(ui);
    let mut cancel = false;
    let mut ok = false;
    ui.add_space(8.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let fill = if danger { p.red } else { p.accent };
        let resp = crate::ui::solid_button(ui, primary, fill, primary_enabled);
        let resp = if primary_enabled {
            resp
        } else {
            resp.on_disabled_hover_text(disabled_reason)
        };
        ok = resp.clicked();
        cancel = ui.button("Cancel").clicked();
    });
    (cancel, ok)
}

/// A pending "are you sure?". Never use it for safe actions.
pub struct Confirm {
    pub title: String,
    pub body: String,
    /// Second line in red (e.g. "It isn't merged; its commits will be lost").
    pub warning: Option<String>,
    /// The red button's label, naming the action ("Delete branch", "Force delete", "Discard").
    pub action: String,
    pub on_confirm: Box<dyn FnOnce(&mut GitYasApp)>,
}

impl Confirm {
    pub fn new(
        title: impl Into<String>,
        body: impl Into<String>,
        action: impl Into<String>,
        on_confirm: impl FnOnce(&mut GitYasApp) + 'static,
    ) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            warning: None,
            action: action.into(),
            on_confirm: Box::new(on_confirm),
        }
    }

    pub fn with_warning(mut self, warning: impl Into<String>) -> Self {
        self.warning = Some(warning.into());
        self
    }
}

/// Draws the open confirmation, if any, and runs its action when confirmed.
pub fn show_confirm(ctx: &egui::Context, app: &mut GitYasApp) {
    let Some(confirm) = app.confirm.as_ref() else { return };
    let p = theme::palette(ctx);
    let outcome = modal(ctx, "confirm", &confirm.title, 400.0, |ui| {
        ui.add(egui::Label::new(&confirm.body).wrap());
        if let Some(warning) = &confirm.warning {
            ui.add(egui::Label::new(RichText::new(warning).color(p.red)).wrap());
        }
        let (cancel, ok) = buttons(ui, &confirm.action, true, true, "");
        (cancel, ok || enter_pressed(ui))
    });
    let (cancel, ok) = outcome.inner;
    if ok {
        if let Some(confirm) = app.confirm.take() {
            (confirm.on_confirm)(app);
        }
    } else if cancel || outcome.closed {
        app.confirm = None;
    }
}
