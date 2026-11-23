//! Status bar (reference UI §1). Shared (WP9): from the left, notifications ‹BELL› (the log opens upwards), HEAD and
//! commit count; on the right the remote-operation progress (WP16's `remote::status_progress`), the zoom level and the
//! GitYas version (opens What's new).

use egui::{Margin, RectAlign, RichText, Ui, WidgetInfo, WidgetType};
use egui_phosphor::regular as icon;

use crate::app::GitYasApp;
use crate::theme;
use crate::ui::{changelog, remote};

pub fn show(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    egui::Panel::bottom("status_bar")
        .exact_size(22.0)
        .frame(egui::Frame::new().fill(p.bg_titlebar).inner_margin(Margin::symmetric(10, 0)))
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                let small = |s: String| RichText::new(s).small().color(p.text_muted);
                bell(ui, app);
                if app.mock {
                    ui.label(RichText::new("MOCK DATA").small().color(p.orange));
                }
                if let Some(repo) = app.store.repo.as_ref() {
                    if let Some(head) = &repo.head {
                        ui.label(small(format!("{}  {}", icon::GIT_BRANCH, head.label())));
                    }
                    if repo.loaded.commits {
                        ui.label(small(format!("{}  {} commits", icon::GIT_COMMIT, repo.commits.len())));
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let version = egui::Label::new(small(format!("GitYas v{}", env!("CARGO_PKG_VERSION")))).sense(egui::Sense::click());
                    if ui.add(version).on_hover_text("What's new").clicked() {
                        changelog::open(app, None);
                    }
                    let zoom = (ui.ctx().zoom_factor() * 100.0).round();
                    ui.label(small(format!("{zoom}%")));
                    remote::status_progress(ui, app);
                });
            });
        });
}

/// ‹BELL› with a red dot for unread errors; opens the notification log above the status bar.
fn bell(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    let resp = ui
        .add(egui::Button::new(RichText::new(icon::BELL).size(13.0).color(p.text_muted)).frame(false))
        .on_hover_text("Notifications");
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Notifications"));
    if app.notify.unread_errors {
        ui.painter()
            .circle_filled(resp.rect.right_top() + egui::vec2(-2.0, 4.0), 3.0, p.red);
    }
    egui::Popup::from_toggle_button_response(&resp)
        .align(RectAlign::TOP_START)
        .show(|ui| {
            app.notify.unread_errors = false;
            crate::notify::log_popover(ui, &mut app.notify);
        });
}
