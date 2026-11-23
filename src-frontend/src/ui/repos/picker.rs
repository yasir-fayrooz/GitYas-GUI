//! The repository popup under the tab bar's `+` and the context bar's repository switcher (reference UI §2).

use egui::{Align2, FontId, Layout, RichText, Sense, Stroke, Ui, UiBuilder, WidgetInfo, WidgetType};
use egui_phosphor::regular as icon;
use gityas_core::models::Repository;
use gityas_core::{Provider, RepoId};

use super::{AddTab, logic};
use crate::app::GitYasApp;
use crate::platform_ui::REVEAL_LABEL;
use crate::theme;

const WIDTH: f32 = 420.0;
const ROW_HEIGHT: f32 = 28.0;

/// Popup state.
#[derive(Default)]
pub struct PickerState {
    pub(super) filter: String,
    /// First frame of the popup: focus the filter and ignore the click that opened it.
    fresh: bool,
}

impl PickerState {
    /// A popup that just opened.
    pub fn new() -> Self {
        Self {
            filter: String::new(),
            fresh: true,
        }
    }
}

/// Drag-and-drop payload: index of the dragged repository in `store.repositories`.
#[derive(Clone, Copy)]
struct RepoDrag(usize);

enum Action {
    Open(RepoId),
    Reveal(RepoId),
    SetProvider(RepoId, Option<Provider>),
    Remove(RepoId),
    Reorder { from: usize, to: usize },
    Add(AddTab),
}

/// Provider icon for rows and menus (Phosphor has no Bitbucket / Azure logos).
pub fn provider_icon(provider: Provider) -> &'static str {
    match provider {
        Provider::Github => icon::GITHUB_LOGO,
        Provider::Gitlab => icon::GITLAB_LOGO,
        Provider::Bitbucket => icon::CLOUD,
        Provider::AzureDevops => icon::WINDOWS_LOGO,
    }
}

/// Draws the popup when open; closes it on Esc or a click outside.
pub fn show(ctx: &egui::Context, app: &mut GitYasApp) {
    if app.confirm.is_some() || app.repos_ui.add.is_some() {
        app.repos_ui.picker = None;
    }
    let Some(mut state) = app.repos_ui.picker.take() else {
        return;
    };
    let anchor = app.repos_ui.plus_anchor.unwrap_or(egui::pos2(12.0, 40.0));
    let p = theme::palette(ctx);
    // A ⋯ menu (its own layer) open at the start of the frame means a click outside the popup went to the menu.
    let menu_was_open = egui::Popup::is_any_open(ctx);
    let max_height = ctx.content_rect().height() * 0.6;

    let mut actions = Vec::new();
    let area = egui::Area::new(egui::Id::new("repo_picker"))
        .order(egui::Order::Foreground)
        .fixed_pos(anchor)
        .constrain(true)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(p.bg_panel)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(6)
                .inner_margin(10)
                .shadow(ui.visuals().popup_shadow)
                .show(ui, |ui| {
                    ui.set_width(WIDTH);
                    contents(ui, app, &mut state, max_height, &mut actions);
                });
        });

    let rect = area.response.rect;
    let outside_click = ctx.input(|i| i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|pos| !rect.contains(pos)));
    let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    let keep = !esc && (state.fresh || menu_was_open || !outside_click);
    state.fresh = false;
    if keep {
        app.repos_ui.picker = Some(state);
    }
    for action in actions {
        apply(app, action);
    }
}

fn apply(app: &mut GitYasApp, action: Action) {
    match action {
        Action::Open(id) => {
            app.repos_ui.picker = None;
            super::switch_to(app, id);
        }
        Action::Reveal(id) => super::reveal(app, &id),
        Action::SetProvider(id, provider) => super::set_provider(app, id, provider),
        Action::Remove(id) => super::confirm_remove(app, id),
        Action::Reorder { from, to } => super::reorder_repository(app, from, to),
        Action::Add(tab) => super::open_add_dialog(app, tab),
    }
}

fn contents(ui: &mut Ui, app: &GitYasApp, state: &mut PickerState, max_height: f32, actions: &mut Vec<Action>) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Repositories").strong());
        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button(format!("{} Clone", icon::DOWNLOAD_SIMPLE))
                .on_hover_text("Clone a repository")
                .clicked()
            {
                actions.push(Action::Add(AddTab::Clone));
            }
            if ui
                .button(format!("{} Open", icon::PLUS))
                .on_hover_text("Open a local repository (Cmd+O)")
                .clicked()
            {
                actions.push(Action::Add(AddTab::Local));
            }
        });
    });
    ui.add_space(4.0);

    let repos = &app.store.repositories;
    if repos.is_empty() {
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("No repositories yet").color(p.text_muted));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Open local repository…").clicked() {
                    actions.push(Action::Add(AddTab::Local));
                }
                if ui.button("Clone repository…").clicked() {
                    actions.push(Action::Add(AddTab::Clone));
                }
            });
        });
        ui.add_space(8.0);
        return;
    }

    let filter = ui.add(
        egui::TextEdit::singleline(&mut state.filter)
            .hint_text(format!("{}  Filter", icon::MAGNIFYING_GLASS))
            .desired_width(f32::INFINITY),
    );
    if state.fresh {
        filter.request_focus();
    }
    let needle = state.filter.trim().to_lowercase();
    let home = logic::home_dir();
    let shown: Vec<(usize, &Repository)> = repos
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            needle.is_empty() || r.name.to_lowercase().contains(&needle) || r.path.to_string_lossy().to_lowercase().contains(&needle)
        })
        .collect();
    if filter.has_focus()
        && ui.input(|i| i.key_pressed(egui::Key::Enter))
        && let Some((_, repo)) = shown.first()
    {
        actions.push(Action::Open(repo.id.clone()));
    }
    ui.add_space(4.0);
    if shown.is_empty() {
        ui.label(RichText::new("No repository matches the filter").color(p.text_muted));
        return;
    }
    // Reordering a filtered list is ambiguous; drag only the full list.
    let can_drag = needle.is_empty();
    egui::ScrollArea::vertical().max_height(max_height).show(ui, |ui| {
        for (index, repo) in shown {
            let row = Row {
                index,
                repo,
                active: app.store.active.as_ref() == Some(&repo.id),
                can_drag,
                path: logic::display_path(&repo.path, home.as_deref()),
            };
            row.show(ui, actions);
        }
    });
}

struct Row<'a> {
    index: usize,
    repo: &'a Repository,
    active: bool,
    can_drag: bool,
    path: String,
}

impl Row<'_> {
    fn show(&self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let p = theme::pal(ui);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_HEIGHT), Sense::hover());
        let id = ui.id().with(("repo_row", &self.repo.id));
        let row = ui.interact(rect, id, Sense::click());
        let name = self.repo.name.clone();
        let active = self.active;
        row.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, active, &name));
        let row = row.on_hover_text(self.repo.path.display().to_string());
        if self.active {
            ui.painter().rect_filled(rect, 4, p.bg_selected);
        } else if row.hovered() {
            ui.painter().rect_filled(rect, 4, p.bg_hover);
        }
        if row.clicked() {
            actions.push(Action::Open(self.repo.id.clone()));
        }
        if self.can_drag {
            if let Some(drag) = row.dnd_hover_payload::<RepoDrag>() {
                let after = ui.ctx().pointer_hover_pos().is_some_and(|pos| pos.y > rect.center().y);
                if drag.0 != self.index {
                    let y = if after { rect.bottom() } else { rect.top() };
                    ui.painter().line_segment(
                        [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                        Stroke::new(2.0, p.accent),
                    );
                }
            }
            if let Some(drag) = row.dnd_release_payload::<RepoDrag>() {
                let after = ui.ctx().pointer_latest_pos().is_some_and(|pos| pos.y > rect.center().y);
                let to = logic::drop_index(drag.0, self.index, after);
                if to != drag.0 {
                    actions.push(Action::Reorder { from: drag.0, to });
                }
            }
        }

        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(rect.shrink2(egui::vec2(4.0, 0.0)))
                .layout(Layout::left_to_right(egui::Align::Center)),
        );
        let ui = &mut child;
        if self.can_drag {
            let handle = ui
                .dnd_drag_source(id.with("drag"), RepoDrag(self.index), |ui| {
                    ui.add(egui::Label::new(RichText::new(icon::DOTS_SIX_VERTICAL).color(p.text_faint)).selectable(false))
                })
                .response;
            handle.on_hover_text("Drag to reorder");
        } else {
            ui.add_space(14.0);
        }
        ui.add(egui::Label::new(RichText::new(icon::FOLDER_SIMPLE).color(p.text_muted)).selectable(false));

        // Menu and provider icon first, so the name and path only get the width they leave.
        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
            let menu = ui.menu_button(RichText::new(icon::DOTS_THREE).size(15.0), |ui| self.menu(ui, actions));
            let label = format!("Actions for {}", self.repo.name);
            menu.response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &label));
            if let Some(provider) = self.repo.provider {
                ui.label(RichText::new(provider_icon(provider)).color(p.text_muted))
                    .on_hover_text(provider.display_name());
            }
            ui.with_layout(Layout::left_to_right(egui::Align::Center), |ui| {
                let name_galley = ui
                    .painter()
                    .layout_no_wrap(self.repo.name.clone(), FontId::proportional(13.0), p.text);
                let name_width = name_galley.size().x.min(170.0).min(ui.available_width().max(0.0));
                let (name_rect, _) = ui.allocate_exact_size(egui::vec2(name_width, ROW_HEIGHT), Sense::hover());
                crate::ui::paint_truncated(
                    ui.painter(),
                    name_rect.left_center(),
                    &self.repo.name,
                    FontId::proportional(13.0),
                    p.text,
                    name_width,
                );
                let width = ui.available_width() - 8.0;
                let font = FontId::proportional(11.5);
                let painter = ui.painter().clone();
                let measure = |s: &str| painter.layout_no_wrap(s.to_owned(), font.clone(), p.text_muted).size().x;
                let text = logic::truncate_left(&self.path, width, measure);
                let (path_rect, _) = ui.allocate_exact_size(egui::vec2(width.max(0.0), ROW_HEIGHT), Sense::hover());
                painter.text(path_rect.right_center(), Align2::RIGHT_CENTER, text, font, p.text_muted);
            });
        });
    }

    fn menu(&self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let p = theme::pal(ui);
        let id = &self.repo.id;
        if ui.button("Open").clicked() {
            actions.push(Action::Open(id.clone()));
        }
        if ui.button(REVEAL_LABEL).clicked() {
            actions.push(Action::Reveal(id.clone()));
        }
        ui.menu_button("Change provider", |ui| {
            let current = self.repo.provider;
            let mut choice = |ui: &mut Ui, provider: Option<Provider>, label: String| {
                if ui.radio(current == provider, label).clicked() && current != provider {
                    actions.push(Action::SetProvider(id.clone(), provider));
                    ui.close();
                }
            };
            choice(ui, None, "None".into());
            for provider in Provider::ALL {
                choice(
                    ui,
                    Some(provider),
                    format!("{}  {}", provider_icon(provider), provider.display_name()),
                );
            }
        });
        ui.separator();
        if ui
            .add(egui::Button::new(RichText::new("Remove from GitYas…").color(p.red)))
            .clicked()
        {
            actions.push(Action::Remove(id.clone()));
        }
    }
}
