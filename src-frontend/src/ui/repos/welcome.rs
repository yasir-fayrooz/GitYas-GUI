//! Centre when no repository is open (reference UI §1 "Welcome"), or the spinner while the first one opens.

use egui::{RichText, Ui};
use egui_phosphor::regular as icon;

use super::{AddTab, logic};
use crate::app::GitYasApp;
use crate::theme;

/// Recent repositories shown. "Recent" is the registry's sort order: the engine has no `last_opened_at` yet.
const RECENT: usize = 8;

pub fn show(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    let mut open = None;
    let mut add = None;
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(p.bg_graph))
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.25).max(24.0));
                if let Some(label) = super::opening_label(app) {
                    ui.add(egui::Spinner::new().size(28.0).color(p.accent));
                    ui.add_space(8.0);
                    ui.label(RichText::new(label).color(p.text_muted));
                    return;
                }
                ui.label(RichText::new(icon::FOLDER_OPEN).size(56.0).color(p.text_faint));
                ui.add_space(8.0);
                ui.label(RichText::new("No repository open").heading().color(p.text_muted));
                ui.add_space(16.0);
                // Centre the two buttons: measure them first (a horizontal layout can't centre itself).
                let width = 2.0 * 170.0 + ui.spacing().item_spacing.x;
                ui.allocate_ui_with_layout(egui::vec2(width, 28.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let size = egui::vec2(170.0, 28.0);
                    if ui.add_sized(size, egui::Button::new("Open local repository…")).clicked() {
                        add = Some(AddTab::Local);
                    }
                    if ui.add_sized(size, egui::Button::new("Clone repository…")).clicked() {
                        add = Some(AddTab::Clone);
                    }
                });

                let repos = &app.store.repositories;
                if repos.is_empty() {
                    return;
                }
                ui.add_space(28.0);
                ui.label(RichText::new("Recent repositories").small().color(p.text_faint));
                ui.add_space(4.0);
                let home = logic::home_dir();
                for repo in repos.iter().take(RECENT) {
                    let path = logic::display_path(&repo.path, home.as_deref());
                    let resp = ui
                        .add(egui::Button::new(RichText::new(&repo.name).color(p.accent)).frame(false))
                        .on_hover_text(path);
                    if resp.clicked() {
                        open = Some(repo.id.clone());
                    }
                }
            });
        });
    if let Some(tab) = add {
        super::open_add_dialog(app, tab);
    }
    if let Some(id) = open {
        super::switch_to(app, id);
    }
}
