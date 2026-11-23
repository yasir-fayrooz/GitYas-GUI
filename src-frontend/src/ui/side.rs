//! Side panels (shared, WP9): the Changes panel pinned on the left (working-tree changes and the commit box, or the
//! selected commit / stash's details), the Branches panel pinned on the right, and the drawer that slides out on the
//! far right for the Settings page (opened from the context bar). What each page draws belongs to the screen modules
//! (`worktree`, `history`, `settings`).
//!
//! The pinned panels start at their minimum width; every panel then keeps the width it was last dragged to, across
//! launches ([`restore_width`]).
//!
//! Working-tree changes select the WIP for review, except while the merge editor is open (it hides the Changes
//! panel while it is the centre view).

use egui::containers::panel::PanelState;
use egui::{Margin, RichText, Ui, WidgetInfo, WidgetType};
use egui_phosphor::regular as icon;
use gityas_core::AppEvent;

use crate::app::{Center, GitYasApp, Selection, SidePage};
use crate::theme;
use crate::ui::{history, settings, worktree};

impl SidePage {
    /// Default width and resize range; each page remembers its own width.
    fn size(self) -> (f32, std::ops::RangeInclusive<f32>) {
        match self {
            SidePage::Changes => (280.0, 280.0..=560.0),
            SidePage::Branches => (220.0, 220.0..=480.0),
            SidePage::Settings => (460.0, 380.0..=720.0),
        }
    }
}

pub fn on_event(app: &mut GitYasApp, event: &AppEvent) {
    if let AppEvent::GitFilesChanged { repo, files } = event {
        let changed = app.layout.files_changed(repo, files);
        let in_merge = app.tab().is_some_and(|t| matches!(t.center, Center::Merge(_)));
        if changed
            && !in_merge
            && app.store.active.as_ref() == Some(repo)
            && let Some(tab) = app.tab_mut()
        {
            tab.selection = Selection::Wip;
        }
    }
}

/// Set by a drawer page's close button ([`page_header`]) while it is drawn; read once the drawer is done.
#[derive(Clone, Copy, Default)]
struct CloseDrawer(bool);

fn drawer_id(page: SidePage) -> egui::Id {
    egui::Id::new(("drawer", page))
}

fn width_id(page: SidePage) -> egui::Id {
    egui::Id::new(("side_width", page))
}

/// Makes the panel of `page` open at the width it was last dragged to (its default until then).
///
/// egui's own panel state is not enough: it stores whatever width the panel was squeezed to, so a minimised or
/// briefly narrow window would shrink the panels to their minimum for good. The width the user chose is kept apart
/// ([`remember_width`]) and put back before the panel is drawn.
fn restore_width(ctx: &egui::Context, page: SidePage) {
    let (default, range) = page.size();
    let id = drawer_id(page);
    let width = ctx
        .data_mut(|d| d.get_persisted::<f32>(width_id(page)))
        .unwrap_or(default)
        .clamp(*range.start(), *range.end());
    let state = PanelState::load(ctx, id);
    if state.is_none_or(|s| (s.size().x - width).abs() > 0.5) {
        let rect = state.map_or(egui::Rect::ZERO, |s| s.outer_rect);
        let outer_rect = egui::Rect::from_min_size(rect.min, egui::vec2(width, rect.height()));
        ctx.data_mut(|d| d.insert_persisted(id, PanelState { outer_rect }));
    }
}

/// Keeps the width of `page`'s panel once the user lets go of its resize edge.
fn remember_width(ctx: &egui::Context, page: SidePage) {
    let id = drawer_id(page);
    let released = ctx.read_response(id.with("__resize")).is_some_and(|r| r.drag_stopped());
    if let Some(state) = PanelState::load(ctx, id).filter(|_| released) {
        ctx.data_mut(|d| d.insert_persisted(width_id(page), state.size().x));
    }
}

/// The drawer and the Branches panel (right) and the Changes panel (left). Call before the centre view.
pub fn panels(ui: &mut Ui, app: &mut GitYasApp) {
    drawer(ui, app);
    branches(ui, app);
    changes(ui, app);
}

/// The right-hand drawer (Settings). Opening and closing slide.
fn drawer(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    let page = SidePage::Settings;
    let mut expanded = app.layout.drawer.is_some();
    let (default, range) = page.size();
    restore_width(ui.ctx(), page);
    ui.data_mut(|d| d.insert_temp(egui::Id::NULL.with("close_drawer"), CloseDrawer(false)));
    egui::Panel::right(drawer_id(page))
        .resizable(true)
        .drag_to_open(false)
        .default_size(default)
        .size_range(range)
        .frame(egui::Frame::new().fill(p.bg_panel).stroke(egui::Stroke::new(1.0, p.border)))
        .show_collapsible(ui, &mut expanded, |ui| padded(ui, |ui| settings::page(ui, app)));
    remember_width(ui.ctx(), page);
    let close = ui
        .data(|d| d.get_temp::<CloseDrawer>(egui::Id::NULL.with("close_drawer")))
        .unwrap_or_default();
    if close.0 {
        app.layout.drawer = None;
    }
}

/// The Branches panel, pinned on the right (inside the drawer).
fn branches(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    let page = SidePage::Branches;
    let mut expanded = app.layout.showing(page) && app.store.repo.is_some();
    let (default, range) = page.size();
    restore_width(ui.ctx(), page);
    egui::Panel::right(drawer_id(page))
        .resizable(true)
        .drag_to_open(false)
        .default_size(default)
        .size_range(range)
        .frame(egui::Frame::new().fill(p.bg_panel).stroke(egui::Stroke::new(1.0, p.border)))
        .show_collapsible(ui, &mut expanded, |ui| {
            ui.set_max_width(ui.available_width());
            history::sidebar(ui, app);
        });
    remember_width(ui.ctx(), page);
}

/// The Changes panel, pinned on the left.
fn changes(ui: &mut Ui, app: &mut GitYasApp) {
    let p = theme::pal(ui);
    let page = SidePage::Changes;
    let mut expanded = app.layout.showing(page) && app.store.repo.is_some();
    let (default, range) = page.size();
    restore_width(ui.ctx(), page);
    egui::Panel::left(drawer_id(page))
        .resizable(true)
        .drag_to_open(false)
        .default_size(default)
        .size_range(range)
        .frame(egui::Frame::new().fill(p.bg_panel))
        .show_collapsible(ui, &mut expanded, |ui| {
            let selection = app.tab().map(|t| t.selection.clone()).unwrap_or_default();
            egui::Frame::new().inner_margin(Margin::same(12)).show(ui, |ui| {
                // Children never push the panel wider than its stored width (a wide child would grow it each frame).
                ui.set_max_width(ui.available_width());
                match selection {
                    Selection::Wip => worktree::panel(ui, app),
                    Selection::Commit(_) | Selection::Stash(_) => history::details(ui, app),
                }
            });
        });
    remember_width(ui.ctx(), page);
    // `expanded` comes back false when the edge is dragged below the minimum width or double-clicked; that is
    // ignored, so resizing stops at the minimum.
}

fn padded(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new().inner_margin(Margin::same(12)).show(ui, add);
}

/// Title row of the drawer page (Settings) with a close button; `right` draws right-aligned controls
/// before it.
pub fn page_header(ui: &mut Ui, title: &str, right: impl FnOnce(&mut Ui)) {
    let p = theme::pal(ui);
    crate::ui::trailing_row(
        ui,
        |ui| {
            let close = crate::ui::icon_button(ui, icon::X, "Close panel");
            close.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Close panel"));
            if close.clicked() {
                ui.data_mut(|d| d.insert_temp(egui::Id::NULL.with("close_drawer"), CloseDrawer(true)));
            }
            right(ui);
        },
        |ui| {
            ui.add(egui::Label::new(RichText::new(title).strong().size(15.0).color(p.text)).truncate());
        },
    );
    ui.add_space(6.0);
}

#[cfg(test)]
mod tests {
    use egui::accesskit::Role;
    use egui_kittest::kittest::Queryable;
    use gityas_core::models::{Delta, DeltaChange, FileState};

    use super::*;
    use crate::app::LayoutState;
    use crate::testutil::TestApp;

    fn files(paths: &[&str]) -> FileState {
        FileState {
            unstaged: paths.iter().map(|p| Delta::new(*p, DeltaChange::Modified)).collect(),
            staged: vec![],
        }
    }

    fn click(t: &mut TestApp, label: &str) {
        t.harness.get_by_role_and_label(Role::Button, label).click();
        t.harness.step();
        t.harness.step();
    }

    fn steps(t: &mut TestApp, n: usize) {
        for _ in 0..n {
            t.harness.step();
        }
    }

    #[test]
    #[ignore = "needs a GPU"]
    fn snapshot_drawer_pages() {
        let mut t = TestApp::mock_gpu();
        t.harness.step();
        for (page, name) in [(SidePage::Branches, "side_branches"), (SidePage::Settings, "side_settings")] {
            t.app().layout.show(page);
            steps(&mut t, 3);
            t.snapshot(name);
        }
    }

    #[test]
    fn only_a_real_change_with_files_counts() {
        let mut layout = LayoutState::default();
        let (a, b) = ("a".to_string(), "b".to_string());
        assert!(!layout.files_changed(&a, &files(&["x"])), "the first state after opening");
        assert!(!layout.files_changed(&a, &files(&["x"])), "re-sent state");
        assert!(layout.files_changed(&a, &files(&["x", "y"])));
        assert!(!layout.files_changed(&a, &files(&[])), "nothing left to review");
        assert!(!layout.files_changed(&b, &files(&["z"])), "another tab's first state");
    }

    #[test]
    fn settings_gear_toggles_the_drawer_and_its_close_button_closes_it() {
        let mut t = TestApp::mock();
        t.harness.step();
        assert!(t.app().layout.showing(SidePage::Changes) && t.app().layout.drawer.is_none());
        click(&mut t, "Settings");
        assert!(t.app().layout.showing(SidePage::Settings));
        // The drawer is on the right.
        let close = t.harness.get_by_role_and_label(Role::Button, "Close panel").rect();
        assert!(close.right() > 1200.0, "{close:?}");
        click(&mut t, "Close panel");
        assert!(t.app().layout.drawer.is_none());
        click(&mut t, "Settings");
        click(&mut t, "Settings");
        assert!(t.app().layout.drawer.is_none(), "the gear closes it again");
    }

    #[test]
    fn changes_panel_keeps_its_width() {
        let mut t = TestApp::mock();
        steps(&mut t, 3);
        let edge = |t: &mut TestApp| t.harness.get_by_role_and_label(Role::Button, "Stage all changes").rect().right();
        let before = edge(&mut t);
        steps(&mut t, 30);
        let after = edge(&mut t);
        assert!((after - before).abs() < 0.5, "the panel grew from {before} to {after}");
        assert!(after < 500.0, "the panel is at its default width, not half the window: {after}");

        // Commit details (long messages, file lists) don't widen it either.
        t.harness.get_by_label("Merge branch 'feature/egui-frontend'").click();
        steps(&mut t, 3);
        let close = |t: &mut TestApp| t.harness.get_by_label("Back to the working tree").rect().right();
        let before = close(&mut t);
        steps(&mut t, 30);
        let after = close(&mut t);
        assert!(
            (after - before).abs() < 0.5 && after < 500.0,
            "details grew the panel from {before} to {after}"
        );
    }

    #[test]
    fn dragging_the_graph_bar_resizes_the_lanes_and_double_click_fits_them_again() {
        let mut t = TestApp::mock();
        steps(&mut t, 3);
        t.harness.ctx.options_mut(|o| o.input_options.max_double_click_delay = 10.0);
        let bar = t.harness.get_by_label("Resize graph").rect();
        let start = bar.center();
        t.harness.hover_at(start);
        t.harness.step();
        t.harness.drag_at(start);
        t.harness.step();
        for dx in [20.0, 60.0, 100.0] {
            t.harness.hover_at(start + egui::vec2(dx, 0.0));
            t.harness.step();
        }
        t.harness.drop_at(start + egui::vec2(100.0, 0.0));
        steps(&mut t, 2);
        let moved = t.harness.get_by_label("Resize graph").rect();
        assert!(moved.center().x > start.x + 80.0, "bar at {moved:?}, was {bar:?}");
        let stored = t.harness.ctx.data_mut(|d| d.get_persisted::<f32>(egui::Id::new("graph_width")));
        assert!(stored.is_some_and(|w| w > 0.0));

        t.harness.get_by_label("Resize graph").click();
        t.harness.step();
        t.harness.get_by_label("Resize graph").click();
        steps(&mut t, 2);
        let back = t.harness.get_by_label("Resize graph").rect();
        assert!((back.center().x - start.x).abs() < 1.0, "double-click fits the lanes: {back:?}");
    }

    #[test]
    fn pinned_panels_start_at_their_minimum_and_keep_a_dragged_width() {
        let mut t = TestApp::mock();
        steps(&mut t, 3);
        let width = |t: &mut TestApp, page: SidePage| PanelState::load(&t.harness.ctx, drawer_id(page)).map_or(0.0, |s| s.size().x);
        assert_eq!(width(&mut t, SidePage::Changes), 280.0);
        assert_eq!(width(&mut t, SidePage::Branches), 220.0);

        // Drag the Changes panel's edge 100 points to the right.
        let start = egui::pos2(280.0, 400.0);
        t.harness.hover_at(start);
        t.harness.step();
        t.harness.drag_at(start);
        t.harness.step();
        for dx in [20.0, 60.0, 100.0] {
            t.harness.hover_at(start + egui::vec2(dx, 0.0));
            t.harness.step();
        }
        t.harness.drop_at(start + egui::vec2(100.0, 0.0));
        steps(&mut t, 2);
        let dragged = width(&mut t, SidePage::Changes);
        assert!((dragged - 380.0).abs() < 2.0, "{dragged}");
        let stored = t.harness.ctx.data_mut(|d| d.get_persisted::<f32>(width_id(SidePage::Changes)));
        assert_eq!(stored, Some(dragged));

        // A window squeezed to nothing (minimised) does not lose it.
        let size = t.harness.ctx.content_rect().size();
        t.harness.set_size(egui::vec2(120.0, 200.0));
        steps(&mut t, 2);
        t.harness.set_size(size);
        steps(&mut t, 2);
        assert_eq!(width(&mut t, SidePage::Changes), dragged);
    }

    #[test]
    fn changes_title_leaves_room_for_the_view_toggle() {
        let mut t = TestApp::mock();
        steps(&mut t, 3);
        let title = t.harness.get_by_role_and_label(Role::Label, "Changes").rect();
        let path = t.harness.get_by_label_contains("Path").rect();
        assert!(title.right() <= path.left(), "title {title:?} runs under the toggle {path:?}");
    }

    #[test]
    fn status_bar_bell_opens_the_log_and_branches_page_creates_branches() {
        let mut t = TestApp::mock();
        t.harness.step();
        t.app().notify.info("Fetched origin");
        click(&mut t, "Notifications");
        // The toast and the log entry.
        assert_eq!(t.harness.query_all_by_label("Fetched origin").count(), 2);
        t.harness.key_press(egui::Key::Escape);
        t.harness.step();
        t.app().layout.show(SidePage::Branches);
        steps(&mut t, 2);
        t.harness.get_by_label_contains("Create branch").click();
        t.harness.step();
        assert!(t.app().history_ui.create_branch_open());
    }

    #[test]
    fn file_changes_select_the_working_tree_except_in_the_merge_editor() {
        let mut t = TestApp::mock();
        t.harness.step();
        let repo = t.app().store.active.clone().unwrap_or_else(|| panic!("no repo"));
        let initial = t.app().store.repo.as_ref().map(|r| r.files.clone()).unwrap_or_default();
        let event = |files: FileState| AppEvent::GitFilesChanged { repo: repo.clone(), files };
        t.app().handle_event(event(initial));
        let commit = t
            .app()
            .store
            .repo
            .as_ref()
            .and_then(|r| r.commits.commits.first())
            .map(|c| c.id.clone());
        let commit = commit.unwrap_or_else(|| panic!("no commits"));
        let select = |t: &mut TestApp| {
            if let Some(tab) = t.app().tab_mut() {
                tab.selection = Selection::Commit(commit.clone());
            }
        };
        select(&mut t);
        t.app().handle_event(event(files(&["new.txt"])));
        assert_eq!(t.app().tab().map(|t| t.selection.clone()), Some(Selection::Wip));

        select(&mut t);
        if let Some(tab) = t.app().tab_mut() {
            tab.center = Center::Merge("src/main.rs".into());
        }
        t.app().handle_event(event(files(&["other.txt"])));
        assert!(
            matches!(t.app().tab().map(|t| &t.selection), Some(Selection::Commit(_))),
            "the merge editor keeps its layout"
        );
    }
}
