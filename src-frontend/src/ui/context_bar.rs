//! Context bar (shared): the strip under the tab bar. From the left: the profile switcher (switch, add, rename
//! or delete profiles: `ui::profiles`), then the git actions (`git_actions`); the Settings gear sits at the far right
//! and toggles the drawer. Branches, tags and stashes live in the pinned Branches panel; repositories in the tab bar.

use egui::{Align2, Color32, FontId, Margin, Rect, Sense, Stroke, Ui, WidgetInfo, WidgetType};
use egui_phosphor::regular as icon;

use crate::app::{GitYasApp, SidePage};
use crate::theme::{self, Palette};
use crate::ui::{git_actions, profiles};

const BAR_HEIGHT: f32 = 58.0;
const CHIP_HEIGHT: f32 = 42.0;
const ICON_BOX: f32 = 30.0;
/// Widest the profile switcher's text may get before it is elided.
const MAX_TEXT_W: f32 = 200.0;

/// Draws the bar once the app has started (without a repository: the profile switcher and the gear only).
pub fn show(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    egui::Panel::top("context_bar")
        .exact_size(BAR_HEIGHT)
        .frame(
            egui::Frame::new()
                .fill(p.bg_toolbar)
                .inner_margin(Margin::symmetric(12, 0))
                .stroke(Stroke::new(1.0, p.border)),
        )
        .show(ui, |ui| {
            // The git actions are centred on the whole bar, not on what is left beside the profile switcher.
            let bar_center = ui.max_rect().center().x;
            ui.horizontal_centered(|ui| {
                // The gear first (right-aligned), so the rest gets whatever width is left.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    settings_gear(ui, app);
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.set_clip_rect(ui.max_rect().intersect(ui.clip_rect()));
                        if app.store.repo.is_none() {
                            profile_switcher(ui, app, MAX_TEXT_W);
                            return;
                        }
                        let profile = profile_chip(app, p);
                        let avail = ui.available_width();
                        let fit = FITS
                            .iter()
                            .find(|fit| row_width(ui, app, &profile, fit) <= avail)
                            .unwrap_or(&FITS[FITS.len() - 1]);
                        profile_switcher(ui, app, fit.switcher_text);
                        let actions = git_actions::width(ui, app, fit.actions);
                        let centred = bar_center - actions / 2.0 - ui.cursor().left();
                        let room = ui.available_width() - actions;
                        ui.add_space(centred.min(room).max(MIN_GAP));
                        ui.spacing_mut().item_spacing.x = git_actions::BUTTON_GAP;
                        git_actions::show(ui, app, fit.actions);
                    });
                });
            });
        });
}

/// Least space between the profile switcher and the git actions (when the window is too narrow to centre them).
const MIN_GAP: f32 = 20.0;

/// One way to lay the bar out. [`show`] takes the first of [`FITS`] that fits the width, so a narrowing window
/// first drops the action labels, then shortens the profile switcher, then folds the actions into one menu.
struct Fit {
    /// Widest text of the profile switcher; 0 = icon only.
    switcher_text: f32,
    actions: git_actions::Density,
}

const FITS: [Fit; 5] = [
    Fit {
        switcher_text: MAX_TEXT_W,
        actions: git_actions::Density::Full,
    },
    Fit {
        switcher_text: MAX_TEXT_W,
        actions: git_actions::Density::Icons,
    },
    Fit {
        switcher_text: 120.0,
        actions: git_actions::Density::Icons,
    },
    Fit {
        switcher_text: 120.0,
        actions: git_actions::Density::Menu,
    },
    Fit {
        switcher_text: 0.0,
        actions: git_actions::Density::Menu,
    },
];

/// Least width of the bar's left part (profile switcher, gap, actions) laid out as `fit`.
fn row_width(ui: &Ui, app: &GitYasApp, profile: &Chip, fit: &Fit) -> f32 {
    chip_width(ui, profile, fit.switcher_text) + ui.spacing().item_spacing.x + MIN_GAP + git_actions::width(ui, app, fit.actions)
}

/// ‹GEAR›: opens / closes the Settings drawer.
fn settings_gear(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    let active = app.layout.showing(SidePage::Settings);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(36.0, 36.0), Sense::click());
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, active, "Settings"));
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, 10, p.accent.gamma_multiply(0.2));
    } else if resp.hovered() {
        painter.rect_filled(rect, 10, p.bg_hover);
    }
    let color = match (active, resp.hovered()) {
        (true, _) => p.accent,
        (false, true) => p.text,
        (false, false) => p.text_muted,
    };
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        icon::GEAR_SIX,
        FontId::proportional(19.0),
        color,
    );
    if resp.on_hover_text("Settings (Cmd+,)").clicked() {
        app.layout.toggle(SidePage::Settings);
    }
}

/// Two-line chip: a tinted icon box, a strong title with a caret and a coloured subtitle. Painted (not labels), so
/// its only accessible name is `name`.
struct Chip {
    glyph: &'static str,
    tint: Color32,
    title: String,
    subtitle: String,
    subtitle_color: Color32,
    name: &'static str,
    tooltip: &'static str,
}

const TITLE_SIZE: f32 = 14.5;
const SUBTITLE_SIZE: f32 = 11.5;

/// Width of the chip's text column, at most `max_text` (0: the chip is drawn without text).
fn chip_text_width(ui: &Ui, c: &Chip, max_text: f32) -> f32 {
    if max_text <= 0.0 {
        return 0.0;
    }
    let measure = |text: &str, size: f32| {
        ui.painter()
            .layout_no_wrap(text.to_owned(), FontId::proportional(size), Color32::WHITE)
            .size()
            .x
    };
    measure(&c.title, TITLE_SIZE).max(measure(&c.subtitle, SUBTITLE_SIZE)).min(max_text)
}

/// Width [`chip`] takes with `max_text`.
fn chip_width(ui: &Ui, c: &Chip, max_text: f32) -> f32 {
    if max_text <= 0.0 {
        // Icon box and caret.
        6.0 + ICON_BOX + 4.0 + 10.0 + 8.0
    } else {
        6.0 + ICON_BOX + 10.0 + chip_text_width(ui, c, max_text) + 8.0 + 12.0 + 8.0
    }
}

/// Draws `c` with its text cut to `max_text` (elided), or as its icon box alone (`max_text` 0; the title and
/// subtitle then lead the tooltip).
fn chip(ui: &mut Ui, c: &Chip, max_text: f32) -> egui::Response {
    let p = theme::pal(ui);
    let title_font = FontId::proportional(TITLE_SIZE);
    let sub_font = FontId::proportional(SUBTITLE_SIZE);
    let text_w = chip_text_width(ui, c, max_text);
    let width = chip_width(ui, c, max_text);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, CHIP_HEIGHT), Sense::click());
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, c.name));
    let painter = ui.painter();
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    if resp.hovered() || open {
        painter.rect_filled(rect, 10, p.bg_hover);
    }
    let icon_rect = Rect::from_center_size(
        egui::pos2(rect.left() + 6.0 + ICON_BOX / 2.0, rect.center().y),
        egui::vec2(ICON_BOX, ICON_BOX),
    );
    painter.rect_filled(icon_rect, 8, c.tint.gamma_multiply(0.18));
    painter.text(
        icon_rect.center(),
        Align2::CENTER_CENTER,
        c.glyph,
        FontId::proportional(16.0),
        c.tint,
    );
    if max_text <= 0.0 {
        painter.text(
            egui::pos2(icon_rect.right() + 4.0, rect.center().y),
            Align2::LEFT_CENTER,
            icon::CARET_DOWN,
            FontId::proportional(11.0),
            p.text_muted,
        );
        return resp.on_hover_text(format!("{} · {}\n{}", c.title, c.subtitle, c.tooltip));
    }
    let text_left = icon_rect.right() + 10.0;
    let title = crate::ui::paint_truncated(
        painter,
        egui::pos2(text_left, rect.center().y - 8.0),
        &c.title,
        title_font,
        p.text,
        text_w,
    );
    painter.text(
        egui::pos2(title.right() + 6.0, title.center().y + 1.0),
        Align2::LEFT_CENTER,
        icon::CARET_DOWN,
        FontId::proportional(11.0),
        p.text_muted,
    );
    crate::ui::paint_truncated(
        painter,
        egui::pos2(text_left, rect.center().y + 9.0),
        &c.subtitle,
        sub_font,
        c.subtitle_color,
        text_w,
    );
    resp.on_hover_text(c.tooltip)
}

fn profile_chip(app: &GitYasApp, p: &Palette) -> Chip {
    let subtitle = match app.store.repositories.len() {
        0 => "No repositories".to_owned(),
        1 => "1 repository".to_owned(),
        n => format!("{n} repositories"),
    };
    Chip {
        glyph: icon::USER_CIRCLE,
        tint: p.accent,
        title: app.profiles_ui.active_name().to_owned(),
        subtitle,
        subtitle_color: p.text_faint,
        name: "Switch profile",
        tooltip: "Switch, add or edit profiles",
    }
}

fn profile_switcher(ui: &mut Ui, app: &mut GitYasApp, max_text: f32) {
    let resp = chip(ui, &profile_chip(app, theme::pal(ui)), max_text);
    egui::Popup::menu(&resp)
        .width(profiles::POPUP_W)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| profiles::menu(ui, app));
}

#[cfg(test)]
mod tests {
    use egui::accesskit::Role;
    use egui_kittest::kittest::Queryable;

    use crate::testutil::TestApp;

    /// Narrow enough that the git actions fold into one menu.
    const NARROW: f32 = 340.0;

    fn click(t: &mut TestApp, label: &str) {
        t.harness.get_by_role_and_label(Role::Button, label).click();
        t.harness.step();
        t.harness.step();
    }

    #[test]
    fn the_bar_has_no_repository_branch_tag_or_stash_switchers() {
        let mut t = TestApp::mock();
        t.harness.step();
        for gone in ["Switch repository", "Switch branch", "Tags", "Stashes"] {
            assert!(
                t.harness.query_by_role_and_label(Role::Button, gone).is_none(),
                "{gone} is still in the context bar"
            );
        }
        t.harness.get_by_role_and_label(Role::Button, "Switch profile");
    }

    #[test]
    fn profile_switcher_lists_the_profiles() {
        let mut t = TestApp::mock();
        t.harness.step();
        click(&mut t, "Switch profile");
        t.harness.get_by_label("Default profile (active)");
        t.harness.get_by_label_contains("New profile");
    }

    #[test]
    fn a_narrow_window_folds_the_git_actions_into_a_menu() {
        const FETCH: &str = "Fetch options";
        const MORE: &str = "Fetch, pull, push, stash and speed mode";
        let mut t = TestApp::mock();
        t.harness.step();
        assert!(t.harness.query_by_label(FETCH).is_some() && t.harness.query_by_label(MORE).is_none());
        // Every action keeps its button (icons only) while it still fits.
        t.harness.set_size(egui::vec2(1080.0, 900.0));
        t.harness.step();
        t.harness.step();
        assert!(t.harness.query_by_label(FETCH).is_some() && t.harness.query_by_label(MORE).is_none());
        t.harness.set_size(egui::vec2(NARROW, 900.0));
        t.harness.step();
        t.harness.step();
        assert!(t.harness.query_by_label(FETCH).is_none());
        let right = t.harness.get_by_role_and_label(Role::Button, MORE).rect().right();
        assert!(right <= NARROW - 36.0, "the menu stays left of the gear ({right})");
        click(&mut t, MORE);
        t.harness.get_by_label_contains("Push");
        t.harness.get_by_label_contains("Pop");
    }

    #[test]
    fn fetch_lives_in_the_dropdown_beside_pull() {
        let mut t = TestApp::mock();
        t.harness.step();
        let pull = t.harness.get_by_label("Pull the current branch (Cmd+Shift+L)").rect();
        let caret = t.harness.get_by_role_and_label(Role::Button, "Fetch options").rect();
        assert!(
            caret.left() >= pull.right() && caret.left() - pull.right() < 4.0,
            "{pull:?} {caret:?}"
        );
        click(&mut t, "Fetch options");
        t.harness.get_by_label("Fetch all remotes (Cmd+Shift+F)");
    }

    #[test]
    #[ignore = "needs a GPU"]
    fn snapshot_context_bar_when_narrow() {
        let mut t = TestApp::mock_gpu();
        for (width, name) in [(1080.0, "context_bar_icons"), (NARROW, "context_bar_menu")] {
            t.harness.set_size(egui::vec2(width, 900.0));
            t.harness.step();
            t.harness.step();
            t.snapshot(name);
        }
    }
}
