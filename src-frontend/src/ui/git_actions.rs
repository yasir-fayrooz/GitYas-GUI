//! Git actions of the context bar (`context_bar`), centred in it: Pull (with a ‹▾› dropdown holding Fetch), Push,
//! then Stash and Pop joined into one two-part button, then Speed mode; labelled, icons only or folded into one menu as the window narrows ([`Density`]). Shared:
//! the layout and button widget live here; what each button does belongs to the screen modules (`remote`, `worktree`).
//! Creating a branch lives on the Branches page and in the branch switcher.

use egui::{Align2, CornerRadius, FontId, Rect, Sense, Ui, WidgetInfo, WidgetType};
use egui_phosphor::regular as icon;

use crate::app::GitYasApp;
use crate::theme;
use crate::ui::{remote, worktree};

pub const BUTTON_HEIGHT: f32 = 30.0;
/// Gap between the Fetch / Pull / Push and Stash / Pop groups.
const GROUP_GAP: f32 = 12.0;
/// Gap between two buttons (the context bar's item spacing while drawing them).
pub const BUTTON_GAP: f32 = 6.0;
/// Index of Stash in [`actions`]: the second group starts there.
const STASH: usize = 3;
/// Index of Fetch in [`actions`]: outside the ‹⋯› menu it lives in the dropdown beside Pull.
const FETCH: usize = 0;
/// Index of Pull in [`actions`].
const PULL: usize = 1;
/// Index of Pop in [`actions`]: joined to Stash.
const POP: usize = 4;
/// Index of Speed mode in [`actions`]: after Stash / Pop.
const SPEED: usize = 5;
/// Width of the ‹▾› dropdown part of the Pull button.
const CARET_WIDTH: f32 = 22.0;

/// How much room the buttons get: labelled, icons only (the label moves to the tooltip), or folded into one ‹⋯›
/// menu when even the icons do not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Density {
    Full,
    Icons,
    Menu,
}

/// One context-bar action button.
pub struct Action<'a> {
    pub glyph: &'a str,
    pub label: &'a str,
    pub tooltip: &'a str,
    /// `Some(reason)` disables the button; the reason is its tooltip (disabled controls say why).
    pub disabled: Option<&'a str>,
    /// Shows a spinner instead of the icon (the operation runs; other buttons stay usable).
    pub running: bool,
    /// Small pill at the top-right (`↓3`, `↑12`, `99+` cap).
    pub badge: Option<String>,
    /// Icon (and badge) only; the label is left to the tooltip.
    pub icon_only: bool,
}

impl<'a> Action<'a> {
    pub fn new(glyph: &'a str, label: &'a str, tooltip: &'a str) -> Self {
        Self {
            glyph,
            label,
            tooltip,
            disabled: None,
            running: false,
            badge: None,
            icon_only: false,
        }
    }
}

/// `↓N` / `↑N` badge text, `None` for zero.
pub fn count_badge(arrow: &str, n: usize) -> Option<String> {
    match n {
        0 => None,
        1..=99 => Some(format!("{arrow}{n}")),
        _ => Some(format!("{arrow}99+")),
    }
}

/// The actions in display order: Fetch, Pull, Push, Stash, Pop, Speed mode.
fn actions(app: &GitYasApp, density: Density) -> [Action<'static>; 6] {
    let repo = app.store.repo.as_ref();
    let no_repo = repo.is_none().then_some("Open a repository first");
    let detached = repo
        .and_then(|r| r.head.as_ref())
        .filter(|h| h.is_detached())
        .map(|_| "HEAD is detached; check out a branch first");
    let unborn = repo
        .and_then(|r| r.head.as_ref())
        .filter(|h| h.is_unborn())
        .map(|_| "No commits yet");
    let no_remote = repo
        .filter(|r| r.loaded.remotes && r.remotes.is_empty())
        .map(|_| "This repository has no remotes");
    let (ahead, behind) = repo.and_then(|r| r.head_branch()).map_or((0, 0), |b| (b.ahead, b.behind));
    let clean = repo.is_some_and(|r| !r.has_wip());
    let no_stash = repo.is_some_and(|r| r.loaded.stashes && r.stashes.is_empty());
    let icon_only = density == Density::Icons;
    [
        Action {
            disabled: no_repo.or(no_remote),
            running: app.bridge.is_running(remote::TASK_FETCH),
            icon_only,
            ..Action::new(icon::ARROWS_CLOCKWISE, "Fetch", "Fetch all remotes (Cmd+Shift+F)")
        },
        Action {
            disabled: no_repo.or(no_remote).or(detached).or(unborn),
            running: app.bridge.is_running(remote::TASK_PULL),
            badge: count_badge("↓", behind),
            icon_only,
            ..Action::new(icon::ARROW_LINE_DOWN, "Pull", "Pull the current branch (Cmd+Shift+L)")
        },
        Action {
            disabled: no_repo.or(no_remote).or(detached).or(unborn),
            running: app.bridge.is_running(remote::TASK_PUSH),
            badge: count_badge("↑", ahead),
            icon_only,
            ..Action::new(icon::ARROW_LINE_UP, "Push", "Push the current branch (Cmd+Shift+P)")
        },
        Action {
            disabled: no_repo.or(clean.then_some("Nothing to stash")),
            icon_only,
            ..Action::new(icon::TRAY_ARROW_DOWN, "Stash", "Stash all changes")
        },
        Action {
            disabled: no_repo.or(no_stash.then_some("No stashes")),
            icon_only,
            ..Action::new(icon::TRAY_ARROW_UP, "Pop", "Pop the latest stash")
        },
        Action {
            icon_only,
            ..Action::new(
                icon::LIGHTNING,
                "Speed mode",
                "Speed mode: shrink GitYas to a small always-on-top panel you drive from the keyboard",
            )
        },
    ]
}

/// Runs action `index` of [`actions`].
fn run(ctx: &egui::Context, app: &mut GitYasApp, index: usize) {
    match index {
        FETCH => remote::fetch_all(app),
        PULL => remote::pull_current(app),
        2 => remote::push_current(app),
        STASH => worktree::open_stash_dialog(app, None),
        SPEED => crate::ui::speed::enter(ctx, app),
        _ => worktree::pop_latest(app),
    }
}

/// The ‹⋯› button standing in for every action at [`Density::Menu`].
fn more_action() -> Action<'static> {
    Action {
        icon_only: true,
        ..Action::new(icon::DOTS_THREE, "More", "Fetch, pull, push, stash and speed mode")
    }
}

/// Width [`show`] takes at `density`.
pub fn width(ui: &Ui, app: &GitYasApp, density: Density) -> f32 {
    if density == Density::Menu {
        return button_width(ui, &more_action());
    }
    let actions = actions(app, density);
    let shown = &actions[PULL..];
    let buttons: f32 = shown.iter().map(|a| button_width(ui, a)).sum();
    // Pull | Push | Stash+Pop | Speed mode: three gaps (Stash and Pop touch), plus the space between the groups.
    buttons + BUTTON_GAP * 3.0 + GROUP_GAP + CARET_WIDTH
}

/// The action buttons, left to right: Pull (with its Fetch dropdown), Push, then Stash / Pop as one joined button (or
/// the ‹⋯› menu holding them all).
pub fn show(ui: &mut Ui, app: &mut GitYasApp, density: Density) {
    let actions = actions(app, density);
    if density == Density::Menu {
        more_menu(ui, app, &actions);
        return;
    }
    let pull_enabled = actions[PULL].disabled.is_none();
    pull_split_button(ui, app, &actions[PULL], &actions[FETCH], pull_enabled);
    if action_button(ui, &actions[2]).clicked() {
        run(ui.ctx(), app, 2);
    }
    ui.add_space(GROUP_GAP);
    if let Some(i) = joined_pair(ui, &actions[STASH], &actions[POP]) {
        run(ui.ctx(), app, if i == 0 { STASH } else { POP });
    }
    if action_button(ui, &actions[SPEED]).clicked() {
        run(ui.ctx(), app, SPEED);
    }
}

/// Two actions drawn as one button split by a thin divider (Stash | Pop); each half is its own button. Returns the
/// half clicked (0 = left, 1 = right).
fn joined_pair(ui: &mut Ui, left: &Action<'_>, right: &Action<'_>) -> Option<usize> {
    let p = theme::pal(ui);
    let left_width = button_width(ui, left);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(left_width + button_width(ui, right), BUTTON_HEIGHT), Sense::hover());
    let (left_rect, right_rect) = rect.split_left_right_at_x(rect.left() + left_width);
    let halves = [
        (
            left,
            left_rect,
            "joined_left",
            CornerRadius {
                nw: 8,
                sw: 8,
                ne: 0,
                se: 0,
            },
        ),
        (
            right,
            right_rect,
            "joined_right",
            CornerRadius {
                nw: 0,
                sw: 0,
                ne: 8,
                se: 8,
            },
        ),
    ];
    let mut clicked = None;
    for (i, (action, half, id, corners)) in halves.into_iter().enumerate() {
        let sense = if action.disabled.is_none() {
            Sense::click()
        } else {
            Sense::hover()
        };
        let resp = ui.interact(half, ui.id().with(id), sense);
        if paint_action(ui, action, half, &resp, corners, false).clicked() {
            clicked = Some(i);
        }
    }
    // One outline around both halves, and a short divider between them.
    let painter = ui.painter();
    painter.rect_stroke(rect, 8, egui::Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
    painter.vline(right_rect.left(), rect.y_range().shrink(7.0), egui::Stroke::new(1.0, p.border));
    clicked
}

/// Pull and its ‹▾› Fetch dropdown, drawn as one button split by a thin divider: the left part pulls, the right
/// part opens a popup offering Fetch (with a spinner while a fetch runs).
fn pull_split_button(ui: &mut Ui, app: &mut GitYasApp, pull: &Action<'_>, fetch: &Action<'_>, pull_enabled: bool) {
    let p = theme::pal(ui);
    let pull_width = button_width(ui, pull);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(pull_width + CARET_WIDTH, BUTTON_HEIGHT), Sense::hover());
    let (left, right) = rect.split_left_right_at_x(rect.left() + pull_width);
    let sense = if pull_enabled { Sense::click() } else { Sense::hover() };
    let pull_resp = ui.interact(left, ui.id().with("pull_split"), sense);
    paint_action(
        ui,
        pull,
        left,
        &pull_resp,
        CornerRadius {
            nw: 8,
            sw: 8,
            ne: 0,
            se: 0,
        },
        false,
    );
    if pull_resp.clicked() {
        run(ui.ctx(), app, PULL);
    }
    pull_resp.context_menu(|ui| {
        if ui.button("Fetch all").clicked() {
            remote::fetch_all(app);
        }
        if ui.add_enabled(pull_enabled, egui::Button::new("Pull")).clicked() {
            remote::pull_current(app);
        }
    });

    let caret = ui.interact(right, ui.id().with("fetch_split"), Sense::click());
    caret.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Fetch options"));
    let caret = caret.on_hover_text("Fetch");
    let painter = ui.painter();
    let corners = CornerRadius {
        nw: 0,
        sw: 0,
        ne: 8,
        se: 8,
    };
    painter.rect_filled(right, corners, if caret.hovered() { p.bg_hover } else { p.bg_panel });
    if fetch.running {
        egui::Spinner::new()
            .size(12.0)
            .color(p.accent)
            .paint_at(ui, Rect::from_center_size(right.center(), egui::vec2(12.0, 12.0)));
    } else {
        let color = if caret.hovered() { p.text } else { p.text_muted };
        painter.text(
            right.center(),
            Align2::CENTER_CENTER,
            icon::CARET_DOWN,
            FontId::proportional(12.0),
            color,
        );
    }
    // One outline around both parts, and a short divider between them.
    painter.rect_stroke(rect, 8, egui::Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
    painter.vline(right.left(), rect.y_range().shrink(7.0), egui::Stroke::new(1.0, p.border));

    let mut clicked = false;
    egui::Popup::menu(&caret).show(|ui| {
        let text = format!("{}  {}", fetch.glyph, fetch.label);
        let item = ui
            .add_enabled(fetch.disabled.is_none(), egui::Button::new(text))
            .on_hover_text(fetch.tooltip)
            .on_disabled_hover_text(fetch.disabled.unwrap_or_default());
        item.widget_info(|| WidgetInfo::labeled(WidgetType::Button, fetch.disabled.is_none(), fetch.tooltip));
        clicked = item.clicked();
    });
    if clicked {
        egui::Popup::close_all(ui.ctx());
        run(ui.ctx(), app, FETCH);
    }
}

/// [`Density::Menu`]: one ‹⋯› button whose popup lists the actions (badges and disabled reasons included).
fn more_menu(ui: &mut Ui, app: &mut GitYasApp, actions: &[Action<'_>]) {
    let resp = action_button(ui, &more_action());
    let mut clicked = None;
    egui::Popup::menu(&resp).show(|ui| {
        for (i, action) in actions.iter().enumerate() {
            if i == STASH || i == SPEED {
                ui.separator();
            }
            let text = match &action.badge {
                Some(badge) => format!("{}  {}   {badge}", action.glyph, action.label),
                None => format!("{}  {}", action.glyph, action.label),
            };
            let item = ui
                .add_enabled(action.disabled.is_none(), egui::Button::new(text))
                .on_hover_text(action.tooltip)
                .on_disabled_hover_text(action.disabled.unwrap_or_default());
            if item.clicked() {
                clicked = Some(i);
            }
        }
    });
    if let Some(i) = clicked {
        egui::Popup::close_all(ui.ctx());
        run(ui.ctx(), app, i);
    }
}

/// Width of [`action_button`] for `action`.
fn button_width(ui: &Ui, action: &Action<'_>) -> f32 {
    let badge_w = action.badge.as_ref().map_or(0.0, |b| badge_galley(ui, b).size().x + 10.0 + 6.0);
    if action.icon_only {
        10.0 + 16.0 + badge_w + 10.0
    } else {
        10.0 + 16.0 + 6.0 + label_galley(ui, action.label).size().x + badge_w + 12.0
    }
}

fn label_galley(ui: &Ui, label: &str) -> std::sync::Arc<egui::Galley> {
    ui.painter()
        .layout_no_wrap(label.to_owned(), FontId::proportional(12.5), theme::pal(ui).text)
}

fn badge_galley(ui: &Ui, badge: &str) -> std::sync::Arc<egui::Galley> {
    ui.painter()
        .layout_no_wrap(badge.to_owned(), FontId::proportional(10.5), egui::Color32::WHITE)
}

/// A pill button: icon and label side by side (or the icon alone), optional spinner / count badge / disabled
/// reason. Its accessible name is the tooltip (the label alone would clash with dialog buttons such as "Stash").
pub fn action_button(ui: &mut Ui, action: &Action<'_>) -> egui::Response {
    let enabled = action.disabled.is_none();
    let width = button_width(ui, action);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, BUTTON_HEIGHT), sense);
    paint_action(ui, action, rect, &resp, CornerRadius::same(8), true)
}

/// Draws `action` into `rect` (its response `resp`): fill, optional outline, icon or spinner, label and badge.
/// Returns `resp` with its hover text.
fn paint_action(ui: &Ui, action: &Action<'_>, rect: Rect, resp: &egui::Response, corners: CornerRadius, outline: bool) -> egui::Response {
    let p = theme::pal(ui);
    let enabled = action.disabled.is_none();
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, action.tooltip));
    // Without a label on the button, its name leads the disabled reason.
    let hover = match (action.disabled, action.icon_only) {
        (Some(reason), true) => format!("{}: {reason}", action.label),
        (Some(reason), false) => reason.to_owned(),
        (None, _) => action.tooltip.to_owned(),
    };
    let resp = resp.clone().on_hover_text(hover);
    let painter = ui.painter();
    let hovered = enabled && resp.hovered();
    let fill = if hovered { p.bg_hover } else { p.bg_panel };
    painter.rect_filled(rect, corners, fill);
    if outline {
        painter.rect_stroke(rect, corners, egui::Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
    }
    let color = match (enabled, hovered) {
        (false, _) => p.text_faint,
        (true, true) => p.text,
        (true, false) => p.text_muted,
    };
    let icon_center = egui::pos2(rect.left() + 10.0 + 8.0, rect.center().y);
    if action.running {
        egui::Spinner::new()
            .size(14.0)
            .color(p.accent)
            .paint_at(ui, Rect::from_center_size(icon_center, egui::vec2(14.0, 14.0)));
    } else {
        let icon_color = if enabled { p.accent } else { p.text_faint };
        painter.text(
            icon_center,
            Align2::CENTER_CENTER,
            action.glyph,
            FontId::proportional(15.0),
            icon_color,
        );
    }
    let mut x = rect.left() + 10.0 + 16.0;
    if !action.icon_only {
        let label = label_galley(ui, action.label);
        x += 6.0;
        painter.galley_with_override_text_color(egui::pos2(x, rect.center().y - label.size().y / 2.0), label.clone(), color);
        x += label.size().x;
    }
    if let Some(badge) = &action.badge {
        let galley = badge_galley(ui, badge);
        let size = galley.size() + egui::vec2(10.0, 3.0);
        let pill = Rect::from_min_size(egui::pos2(x + 6.0, rect.center().y - size.y / 2.0), size);
        painter.rect_filled(pill, size.y / 2.0, if enabled { p.accent } else { p.text_faint });
        painter.galley(pill.center() - galley.size() / 2.0, galley, egui::Color32::WHITE);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::count_badge;

    #[test]
    fn badges_cap_at_99() {
        assert_eq!(count_badge("↓", 0), None);
        assert_eq!(count_badge("↓", 3).as_deref(), Some("↓3"));
        assert_eq!(count_badge("↑", 100).as_deref(), Some("↑99+"));
    }
}
