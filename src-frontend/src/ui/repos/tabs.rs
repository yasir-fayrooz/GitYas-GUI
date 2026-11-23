//! Tab bar (reference UI §1): the GitYas label and `+` (open a repository) on the left, then one tab per open
//! repository: active highlight, click switches, `✕` / middle-click close, drag to reorder, context menu. Tabs keep
//! their width; when they overflow the bar scrolls sideways (mouse wheel, trackpad, or dragging its scroll bar), and
//! a newly active tab is scrolled into view.

use egui::{Align2, FontId, Margin, Rect, Sense, Stroke, Ui, WidgetInfo, WidgetType};
use egui_phosphor::regular as icon;
use gityas_core::RepoId;

use super::{logic, picker};
use crate::app::GitYasApp;
use crate::platform_ui::REVEAL_LABEL;
use crate::theme;

const TAB_WIDTH: f32 = 180.0;
const TAB_HEIGHT: f32 = 26.0;
const BAR_HEIGHT: f32 = 34.0;
/// Space above the tabs (they sit where they would if centred in the bar).
const TAB_TOP: f32 = (BAR_HEIGHT - TAB_HEIGHT) / 2.0;

/// Drag-and-drop payload: index of the dragged tab.
#[derive(Clone, Copy)]
struct TabDrag(usize);

enum Action {
    Switch(RepoId),
    Close(RepoId),
    CloseOthers(RepoId),
    Reveal(RepoId),
    Remove(RepoId),
    Move { from: usize, to: usize },
}

pub fn show(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    egui::Panel::top("repo_tabs")
        .exact_size(BAR_HEIGHT)
        .frame(egui::Frame::new().fill(p.bg_titlebar).inner_margin(Margin::symmetric(8, 0)))
        .show(ui, |ui| {
            let bar = ui.max_rect();
            ui.horizontal_centered(|ui| {
                ui.label(egui::RichText::new(icon::GIT_BRANCH).size(16.0).color(p.accent));
                ui.label(egui::RichText::new("GitYas").strong());
                ui.add_space(4.0);

                let ready = app.store.started && app.store.fatal.is_none();
                let plus = ui.add_enabled(ready, egui::Button::new(egui::RichText::new(icon::PLUS).size(15.0)).frame(false));
                let plus = plus.on_hover_text("Open a repository (Cmd+T)");
                plus.widget_info(|| WidgetInfo::labeled(WidgetType::Button, ready, "Open a repository"));
                app.repos_ui.plus_anchor = Some(plus.rect.left_bottom());
                if plus.clicked() {
                    toggle_picker(app);
                }
                ui.add_space(8.0);

                let mut actions = Vec::new();
                let active = super::shown_active(app).cloned();
                // Scroll a tab into view once, when it becomes the active one.
                let reveal = active.clone().filter(|id| app.repos_ui.revealed_tab.as_ref() != Some(id));
                // The tabs keep a fixed top; the scroll bar (only when they overflow) takes the strip below them
                // instead of floating over them.
                let area = egui::Rect::from_min_max(egui::pos2(ui.cursor().left(), bar.top() + TAB_TOP), bar.right_bottom());
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(area)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                    |ui| {
                        // The vertical wheel scrolls the tabs sideways (the bar only scrolls horizontally).
                        ui.style_mut().always_scroll_the_only_direction = true;
                        ui.spacing_mut().scroll = egui::style::ScrollStyle {
                            floating: false,
                            bar_width: 3.0,
                            bar_inner_margin: 0.5,
                            bar_outer_margin: 0.0,
                            handle_min_length: 24.0,
                            // Faintly visible at rest, so the overflow shows before the pointer is over the tabs.
                            dormant_handle_opacity: 0.35,
                            ..egui::style::ScrollStyle::solid()
                        };
                        tabs_scroll(ui, app, active.as_ref(), reveal.as_ref(), &mut actions);
                    },
                );
                if reveal.is_some() {
                    app.repos_ui.revealed_tab = reveal;
                }

                for action in actions {
                    apply(app, action);
                }
            });
        });
}

fn tabs_scroll(ui: &mut Ui, app: &GitYasApp, active: Option<&RepoId>, reveal: Option<&RepoId>, actions: &mut Vec<Action>) {
    egui::ScrollArea::horizontal()
        .id_salt("repo_tabs_scroll")
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for (index, id) in app.store.open_tabs.open.iter().enumerate() {
                    let name = app.store.repository(id).map_or(id.as_str(), |r| r.name.as_str());
                    let tab = Tab {
                        index,
                        name,
                        active: active == Some(id),
                        opening: app.repos_ui.opening.as_ref() == Some(id),
                        reveal: reveal == Some(id),
                    };
                    if let Some(action) = tab.show(ui, id) {
                        actions.push(action);
                    }
                }
            });
        });
}

fn toggle_picker(app: &mut GitYasApp) {
    if app.repos_ui.picker.is_some() {
        app.repos_ui.picker = None;
    } else {
        app.repos_ui.picker = Some(picker::PickerState::new());
    }
}

fn apply(app: &mut GitYasApp, action: Action) {
    match action {
        Action::Switch(id) => super::switch_to(app, id),
        Action::Close(id) => super::close_tab(app, &id),
        Action::CloseOthers(id) => super::close_other_tabs(app, &id),
        Action::Reveal(id) => super::reveal(app, &id),
        Action::Remove(id) => super::confirm_remove(app, id),
        Action::Move { from, to } => super::move_tab(app, from, to),
    }
}

struct Tab<'a> {
    index: usize,
    name: &'a str,
    active: bool,
    opening: bool,
    /// Scroll this tab into view (it just became active).
    reveal: bool,
}

impl Tab<'_> {
    fn show(&self, ui: &mut Ui, id: &RepoId) -> Option<Action> {
        let p = theme::pal(ui);
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(TAB_WIDTH, TAB_HEIGHT), Sense::click_and_drag());
        let name = self.name.to_owned();
        let active = self.active;
        resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, active, &name));
        let name_space = rect.width() - 30.0 - 28.0;
        let name_width = ui
            .painter()
            .layout_no_wrap(self.name.to_owned(), FontId::proportional(13.0), p.text)
            .size()
            .x;
        // Full name as tooltip only when it is truncated (a tooltip repeating the label is noise).
        let resp = if name_width > name_space {
            resp.on_hover_text(self.name)
        } else {
            resp
        };
        if self.reveal {
            resp.scroll_to_me(None);
        }
        if resp.dragged() {
            resp.dnd_set_drag_payload(TabDrag(self.index));
        }

        let fill = if self.active {
            p.bg_toolbar
        } else if resp.hovered() {
            p.bg_hover
        } else {
            p.bg_titlebar
        };
        let painter = ui.painter();
        painter.rect_filled(
            rect,
            egui::CornerRadius {
                nw: 6,
                ne: 6,
                sw: 0,
                se: 0,
            },
            fill,
        );
        if self.active {
            painter.line_segment([rect.left_top(), rect.right_top()], Stroke::new(2.0, p.accent));
        }
        let text_color = if self.active { p.text } else { p.text_muted };
        let icon_pos = rect.left_center() + egui::vec2(10.0, 0.0);
        if self.opening {
            egui::Spinner::new()
                .size(12.0)
                .color(p.accent)
                .paint_at(ui, Rect::from_center_size(icon_pos + egui::vec2(6.0, 0.0), egui::vec2(12.0, 12.0)));
        } else {
            painter.text(
                icon_pos,
                Align2::LEFT_CENTER,
                icon::FOLDER_SIMPLE,
                FontId::proportional(13.0),
                text_color,
            );
        }
        crate::ui::paint_truncated(
            ui.painter(),
            rect.left_center() + egui::vec2(30.0, 0.0),
            self.name,
            FontId::proportional(13.0),
            text_color,
            name_space,
        );

        // Drop target: insertion marker on the half the pointer is over.
        let mut action = None;
        if let Some(drag) = resp.dnd_hover_payload::<TabDrag>() {
            let after = ui.ctx().pointer_hover_pos().is_some_and(|pos| pos.x > rect.center().x);
            if drag.0 != self.index {
                let x = if after { rect.right() } else { rect.left() };
                ui.painter().line_segment(
                    [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                    Stroke::new(2.0, p.accent),
                );
            }
        }
        if let Some(drag) = resp.dnd_release_payload::<TabDrag>() {
            let after = ui.ctx().pointer_latest_pos().is_some_and(|pos| pos.x > rect.center().x);
            let to = logic::drop_index(drag.0, self.index, after);
            if to != drag.0 {
                action = Some(Action::Move { from: drag.0, to });
            }
        }

        let close_rect = Rect::from_center_size(rect.right_center() - egui::vec2(14.0, 0.0), egui::vec2(18.0, 18.0));
        let close = ui.interact(close_rect, resp.id.with("close"), Sense::click());
        let close_label = format!("Close {}", self.name);
        close.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &close_label));
        let close = close.on_hover_text("Close tab (Cmd+W)");
        if resp.hovered() || close.hovered() || self.active {
            if close.hovered() {
                ui.painter().rect_filled(close_rect, 3, p.bg_hover);
            }
            ui.painter().text(
                close_rect.center(),
                Align2::CENTER_CENTER,
                icon::X,
                FontId::proportional(12.0),
                p.text_muted,
            );
        }

        resp.context_menu(|ui| {
            if ui.button("Close").clicked() {
                action = Some(Action::Close(id.clone()));
            }
            if ui.button("Close others").clicked() {
                action = Some(Action::CloseOthers(id.clone()));
            }
            if ui.button(REVEAL_LABEL).clicked() {
                action = Some(Action::Reveal(id.clone()));
            }
            ui.separator();
            let remove = egui::Button::new(egui::RichText::new("Remove from GitYas…").color(p.red));
            if ui.add(remove).clicked() {
                action = Some(Action::Remove(id.clone()));
            }
        });

        if close.clicked() || resp.middle_clicked() {
            action = Some(Action::Close(id.clone()));
        } else if resp.clicked() && !self.active {
            action = Some(Action::Switch(id.clone()));
        }
        action
    }
}
