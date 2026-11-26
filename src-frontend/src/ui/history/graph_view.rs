//! Commit graph (centre), drawn as a timeline: a toolbar (title, commit count, search), then virtualised two-line
//! commit cards (WIP card, commits, shallow notice) with the lanes on their left, ref chips inline before the
//! message, chip and commit menus, ↑/↓ selection and Cmd+F search highlight.

use std::collections::HashMap;
use std::path::Path;

use egui::{Align2, Color32, FontId, Key, Pos2, Rect, Sense, Shape, Stroke, Ui, WidgetInfo, WidgetType};
use egui_phosphor::regular as icon;
use gityas_core::models::{Branch, ChangeCounts, Commit, DeltaChange, GraphOptions, ResetMode, shorten_refname};

use super::menus::{self, MenuCtx};
use super::model::{Pill, PillKind, Search, TagRow, build_pills, next_match, pills_tooltip};
use super::{HistoryTab, Intent, RemoteTags};
use crate::app::{GitYasApp, Selection};
use crate::graph::{Edge, GraphRow};
use crate::store::RepoData;
use crate::theme::{self, Palette};
use crate::ui::{cmd, paint_avatar, paint_truncated, plain_cursor};

const ROW_H: f32 = 48.0;
const LANE_W: f32 = 16.0;
const TOOLBAR_H: f32 = 48.0;
const SOLO_H: f32 = 30.0;
const NODE_R: f32 = 5.5;
const GRAPH_PAD: f32 = 10.0;
/// Inset of the row cards (hover / selection backgrounds) from the view's edges.
const CARD_INSET: f32 = 8.0;
/// Widest the automatic lane area gets; lanes beyond it pile up on its right edge (drag the resize bar for more).
const MAX_AUTO_GRAPH_W: f32 = 240.0;
/// Resize bar limits: one lane at least.
const MIN_GRAPH_W: f32 = GRAPH_PAD * 2.0 + LANE_W;
const MAX_GRAPH_W: f32 = 520.0;
/// Half the height of a commit's band (the capsule that grows out of its node into the message).
const BAND_R: f32 = 17.0;
/// Right-hand column with the age (line 1) and the short SHA (line 2).
const META_W: f32 = 96.0;
const CHIP_H: f32 = 19.0;
/// Ref chips drawn inline before the message; the rest go behind a `+N` chip.
const MAX_CHIPS: usize = 2;

/// Where things go in one row.
struct RowLayout {
    card: Rect,
    graph: Rect,
    content: Rect,
    meta: Rect,
    /// Furthest a lane centre may sit right of lane 0. Lanes keep their full `LANE_W` spacing up to it and the
    /// ones beyond pile up there.
    max_offset: f32,
    line1: f32,
    line2: f32,
}

/// Width of the lane area: the dragged width, or (`stored == 0`) one that fits the lanes.
fn graph_width(max_lanes: usize, stored: f32) -> f32 {
    if stored > 0.0 {
        stored.clamp(MIN_GRAPH_W, MAX_GRAPH_W)
    } else {
        (max_lanes.max(1) as f32 * LANE_W + GRAPH_PAD * 2.0).min(MAX_AUTO_GRAPH_W)
    }
}

/// Left edge of the lane area relative to the view's left edge (the resize bar sits at this plus its width).
const GRAPH_LEFT: f32 = CARD_INSET + 4.0;

fn graph_width_id() -> egui::Id {
    egui::Id::new("graph_width")
}

fn focus_lane_id() -> egui::Id {
    egui::Id::new("graph_focus_lane")
}

/// How much a lane's edges and bands keep while another lane has the focus.
const UNFOCUSED: f32 = 0.3;

fn lane_alpha(focus: Option<usize>, lane: usize) -> f32 {
    match focus {
        Some(f) if f != lane => UNFOCUSED,
        _ => 1.0,
    }
}

/// The dragged lane-area width (persisted); 0 = automatic.
fn stored_graph_width(ctx: &egui::Context) -> f32 {
    ctx.data_mut(|d| d.get_persisted::<f32>(graph_width_id())).unwrap_or(0.0)
}

impl RowLayout {
    fn new(row: Rect, graph_w: f32) -> Self {
        let card = row.shrink2(egui::vec2(CARD_INSET, 2.0));
        let graph = Rect::from_min_size(egui::pos2(row.left() + GRAPH_LEFT, row.top()), egui::vec2(graph_w, row.height()));
        let meta = Rect::from_min_max(
            egui::pos2(card.right() - META_W - 12.0, row.top()),
            egui::pos2(card.right() - 12.0, row.bottom()),
        );
        let content = Rect::from_min_max(
            egui::pos2(graph.right() + 6.0, row.top()),
            egui::pos2(meta.left() - 12.0, row.bottom()),
        );
        Self {
            card,
            graph,
            content,
            meta,
            max_offset: (graph_w - GRAPH_PAD * 2.0 - LANE_W).max(0.0),
            line1: row.center().y - 9.0,
            line2: row.center().y + 10.0,
        }
    }

    fn lane_x(&self, lane: usize) -> f32 {
        self.graph.left() + GRAPH_PAD + (lane as f32 * LANE_W).min(self.max_offset) + LANE_W / 2.0
    }
}

/// Everything a row needs, borrowed from the store / tab for one frame.
struct RowCtx<'a> {
    p: &'a Palette,
    /// Width of the lane area this frame.
    graph_w: f32,
    /// Lane of the row under the pointer last frame: its edges and band stay bright, the others dim.
    focus_lane: Option<usize>,
    /// Lane of the row under the pointer this frame (stored for the next one).
    hovered_lane: std::cell::Cell<Option<usize>>,
    repo: &'a RepoData,
    opts: &'a GraphOptions,
    menu: MenuCtx,
    search: Search,
    remote_tags: &'a RemoteTags,
}

pub(super) fn show(ui: &mut Ui, app: &mut GitYasApp) -> Vec<Intent> {
    let mut out = Vec::new();
    let p = theme::pal(ui);
    // A pending tab switch shows "Opening <name>…" instead of the previous repository's history, until the engine has
    // switched to it (from then on its data, possibly restored from a recent visit, is shown while it refreshes).
    let switching = crate::ui::repos::switch_pending(app);
    let opening = crate::ui::repos::opening_label(app);
    let GitYasApp { store, tabs, .. } = app;
    let (Some(repo), Some(active)) = (store.repo.as_ref(), store.active.clone()) else {
        return out;
    };
    let opts = &store.settings.graph;
    let tab = tabs.entry(active).or_default();
    let wip_summary = tab.worktree.summary.trim().to_owned();
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(p.bg_graph))
        .show(ui, |ui| {
            if switching || !repo.is_ready() {
                let label = opening.as_deref().unwrap_or("Loading history…");
                centered(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.label(egui::RichText::new(label).color(p.text_muted));
                    });
                });
                return;
            }
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            if ui.input_mut(|i| i.consume_shortcut(&cmd(Key::F))) {
                tab.history.search_open = true;
                tab.history.focus_search = true;
            }
            let graph_w = graph_width(repo.graph.max_lanes, stored_graph_width(ui.ctx()));
            let search_nav = toolbar(ui, p, repo, &mut tab.history);
            if let Some(refname) = &repo.solo {
                solo_bar(ui, p, refname, &mut out);
            }
            // Taken for the frame (rows only read it) so the tab can be borrowed mutably; restored below.
            let remote_tags = std::mem::take(&mut tab.history.remote_tags);
            let ctx = RowCtx {
                p,
                graph_w,
                focus_lane: ui.data(|d| d.get_temp::<Option<usize>>(focus_lane_id())).flatten(),
                hovered_lane: std::cell::Cell::new(None),
                repo,
                opts,
                menu: MenuCtx::new(repo),
                search: Search::new(if tab.history.search_open { &tab.history.search } else { "" }),
                remote_tags: &remote_tags,
            };
            body(ui, &ctx, tab, &wip_summary, search_nav, &mut out);
            drop(ctx);
            tab.history.remote_tags = remote_tags;
        });
    out
}

fn centered(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.4).max(0.0));
        add(ui);
    });
}

/// Search navigation requested from the toolbar (Enter / Shift+Enter in the search field).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchNav {
    Next,
    Previous,
}

const SEARCH_ID: &str = "graph_search_field";

/// Toolbar above the timeline: "History", the commit count and the search field (always visible; Cmd+F focuses it).
/// The bar between the lanes and the commits: drag to resize the lane area, double-click to fit the lanes again. A
/// faint line at rest, accent while hovered or dragged.
fn resize_bar(ui: &mut Ui, p: &Palette, max_lanes: usize, graph_w: f32, rect: Rect) {
    let resp = ui.interact(rect, egui::Id::new("graph_resize_bar"), Sense::click_and_drag());
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Other, true, "Resize graph"));
    let active = resp.hovered() || resp.dragged();
    let x = rect.center().x;
    if active {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeColumn);
        ui.painter().line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            Stroke::new(2.0, p.accent),
        );
    } else {
        ui.painter().line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            Stroke::new(1.0, p.border),
        );
    }
    let resp = resp.on_hover_text("Drag to resize the graph; double-click to fit the lanes");
    let stored = if resp.double_clicked() {
        Some(0.0)
    } else if resp.dragged() && resp.drag_delta().x != 0.0 {
        Some((graph_w + resp.drag_delta().x).clamp(MIN_GRAPH_W, MAX_GRAPH_W))
    } else {
        None
    };
    if let Some(w) = stored {
        // A width equal to the automatic one stays automatic (it follows the lanes again).
        let w = if (w - graph_width(max_lanes, 0.0)).abs() < 0.5 { 0.0 } else { w };
        ui.ctx().data_mut(|d| d.insert_persisted(graph_width_id(), w));
    }
}

fn toolbar(ui: &mut Ui, p: &Palette, repo: &RepoData, history: &mut HistoryTab) -> Option<SearchNav> {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), TOOLBAR_H), Sense::hover());
    let painter = ui.painter().clone();
    let title = painter.text(
        rect.left_center() + egui::vec2(20.0, 0.0),
        Align2::LEFT_CENTER,
        "History",
        FontId::proportional(16.0),
        p.text,
    );
    let n = repo.commits.commits.len();
    painter.text(
        egui::pos2(title.right() + 10.0, rect.center().y + 1.0),
        Align2::LEFT_CENTER,
        format!("{n} {}", if n == 1 { "commit" } else { "commits" }),
        FontId::proportional(12.0),
        p.text_faint,
    );

    // Search field on the right.
    let field_w = (rect.width() * 0.4).clamp(160.0, 320.0);
    let field = Rect::from_min_size(
        egui::pos2(rect.right() - 16.0 - field_w, rect.center().y - 15.0),
        egui::vec2(field_w, 30.0),
    );
    let field_id = egui::Id::new(SEARCH_ID);
    let focused = ui.memory(|m| m.has_focus(field_id));
    painter.rect_filled(field, 8, p.bg_input);
    painter.rect_stroke(
        field,
        8,
        Stroke::new(1.0, if focused { p.accent } else { p.border }),
        egui::StrokeKind::Inside,
    );
    painter.text(
        field.left_center() + egui::vec2(12.0, 0.0),
        Align2::LEFT_CENTER,
        icon::MAGNIFYING_GLASS,
        FontId::proportional(13.0),
        if history.search_open { p.accent } else { p.text_faint },
    );
    let close_rect = Rect::from_center_size(egui::pos2(field.right() - 14.0, field.center().y), egui::vec2(18.0, 18.0));
    let matches = if history.search.trim().is_empty() {
        None
    } else {
        let s = Search::new(&history.search);
        Some(repo.commits.commits.iter().filter(|c| s.matches(c)).count())
    };
    let count_w = if matches.is_some() { 64.0 } else { 0.0 };
    let edit_rect = Rect::from_min_max(
        egui::pos2(field.left() + 30.0, field.top() + 4.0),
        egui::pos2(field.right() - 26.0 - count_w, field.bottom() - 4.0),
    );
    let resp = ui.put(
        edit_rect,
        egui::TextEdit::singleline(&mut history.search)
            .id(field_id)
            .frame(egui::Frame::NONE)
            .vertical_align(egui::Align::Center)
            .hint_text("Search message, author or SHA")
            .desired_width(edit_rect.width()),
    );
    if history.focus_search {
        resp.request_focus();
        history.focus_search = false;
    }
    if resp.changed() || resp.has_focus() {
        history.search_open = true;
    }
    if let Some(n) = matches {
        painter.text(
            egui::pos2(close_rect.left() - 6.0, field.center().y),
            Align2::RIGHT_CENTER,
            if n == 1 { "1 match".to_owned() } else { format!("{n} matches") },
            FontId::proportional(11.0),
            p.text_muted,
        );
    }
    let mut nav = None;
    if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
        nav = Some(if ui.input(|i| i.modifiers.shift) {
            SearchNav::Previous
        } else {
            SearchNav::Next
        });
        resp.request_focus();
    }
    let escape = resp.has_focus() && ui.input(|i| i.key_pressed(Key::Escape));
    let mut clear = escape;
    if !history.search.is_empty() {
        let close = ui.interact(close_rect, egui::Id::new("graph_search_close"), Sense::click());
        close.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Close search"));
        painter.text(
            close_rect.center(),
            Align2::CENTER_CENTER,
            icon::X_CIRCLE,
            FontId::proportional(14.0),
            if close.hovered() { p.text } else { p.text_faint },
        );
        clear |= close.on_hover_text("Clear search (Esc)").clicked();
    }
    if clear {
        history.search_open = false;
        history.search.clear();
        if escape {
            resp.surrender_focus();
        }
    } else if !resp.has_focus() && !history.focus_search && history.search.is_empty() {
        history.search_open = false;
    }
    nav
}

/// Strip under the toolbar while a branch is soloed: which one, and a button to show every branch again.
fn solo_bar(ui: &mut Ui, p: &Palette, refname: &str, out: &mut Vec<Intent>) {
    let (row, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), SOLO_H + 6.0), Sense::hover());
    let rect = row.shrink2(egui::vec2(CARD_INSET + 8.0, 3.0));
    let painter = ui.painter();
    painter.rect_filled(rect, 8, p.accent.gamma_multiply(0.14));
    painter.text(
        rect.left_center() + egui::vec2(12.0, 0.0),
        Align2::LEFT_CENTER,
        format!("{}  Showing only {} and its history", icon::FUNNEL, shorten_refname(refname)),
        FontId::proportional(12.0),
        p.text,
    );
    let button = Rect::from_min_max(
        egui::pos2(rect.right() - 136.0, rect.top() + 3.0),
        egui::pos2(rect.right() - 4.0, rect.bottom() - 3.0),
    );
    if ui.put(button, egui::Button::new("Show all branches")).clicked() {
        out.push(Intent::Solo(None));
    }
}

fn row_of(selection: &Selection, repo: &RepoData, wip_rows: usize) -> Option<usize> {
    match selection {
        Selection::Wip => (wip_rows == 1).then_some(0),
        Selection::Commit(id) => repo.commit_index(id).map(|i| i + wip_rows),
        Selection::Stash(_) => None,
    }
}

fn body(ui: &mut Ui, ctx: &RowCtx<'_>, tab: &mut crate::app::TabState, wip_summary: &str, nav: Option<SearchNav>, out: &mut Vec<Intent>) {
    let repo = ctx.repo;
    let commits = &repo.commits.commits;
    let wip_rows = usize::from(repo.has_wip());
    let shallow_rows = usize::from(repo.status.is_shallow && !commits.is_empty());
    let row_count = wip_rows + commits.len() + shallow_rows;
    let commit_rows = wip_rows + commits.len();

    // Where to scroll: search navigation, a reveal request, or keyboard selection.
    let mut target: Option<usize> = None;
    if let Some(nav) = nav {
        let from = match &tab.selection {
            Selection::Commit(id) => repo.commit_index(id),
            _ => None,
        };
        if let Some(i) = next_match(commits, &ctx.search, from, nav == SearchNav::Previous)
            && let Some(c) = commits.get(i)
        {
            tab.selection = Selection::Commit(c.id.clone());
            target = Some(i + wip_rows);
        }
    }
    if let Some(id) = tab.history.scroll_to.take() {
        target = repo.commit_index(&id).map(|i| i + wip_rows);
    }
    let focused = ui.ctx().memory(|m| m.focused().is_some());
    if !focused && commit_rows > 0 {
        let (up, down) = ui.input(|i| (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown)));
        if up || down {
            let current = row_of(&tab.selection, repo, wip_rows);
            let next = match (current, up) {
                (None, _) => 0,
                (Some(c), true) => c.saturating_sub(1),
                (Some(c), false) => (c + 1).min(commit_rows - 1),
            };
            tab.selection = if next < wip_rows {
                Selection::Wip
            } else {
                commits
                    .get(next - wip_rows)
                    .map_or(Selection::Wip, |c| Selection::Commit(c.id.clone()))
            };
            target = Some(next);
        }
    }

    let scroll_id = egui::Id::new("commit_graph_scroll");
    let mut scroll = egui::ScrollArea::vertical().id_salt(scroll_id).auto_shrink(false);
    if let Some(row) = target {
        let (offset, viewport_h) = ui.data(|d| d.get_temp::<(f32, f32)>(scroll_id)).unwrap_or_default();
        let top = row as f32 * ROW_H;
        let bottom = top + ROW_H;
        if top < offset || bottom > offset + viewport_h {
            scroll = scroll.vertical_scroll_offset((top - (viewport_h - ROW_H) / 2.0).max(0.0));
        }
    }
    let output = scroll.show_rows(ui, ROW_H, row_count, |ui, range| {
        for index in range {
            if index < wip_rows {
                let selected = tab.selection == Selection::Wip;
                if wip_row(ui, ctx, wip_summary, selected).clicked() {
                    out.push(Intent::Select(Selection::Wip));
                }
            } else if let Some(commit) = commits.get(index - wip_rows) {
                let graph_row = repo.graph.rows.get(index - wip_rows);
                let selected = tab.selection == Selection::Commit(commit.id.clone());
                let resp = commit_row(ui, ctx, commit, graph_row, selected, out);
                if resp.contains_pointer() {
                    crate::ui::side::mark_commit_row(ui.ctx());
                }
                if resp.clicked() {
                    out.push(Intent::Select(Selection::Commit(commit.id.clone())));
                }
            } else {
                shallow_row(ui, ctx.p);
            }
        }
    });
    ui.data_mut(|d| d.insert_temp(scroll_id, (output.state.offset.y, output.inner_rect.height())));
    if let Some(head) = repo.head_target()
        && let Some(index) = repo.commit_index(head)
    {
        let top = (index + wip_rows) as f32 * ROW_H;
        let (offset, view_h) = (output.state.offset.y, output.inner_rect.height());
        // Off screen once less than half of its row shows.
        let above = top + ROW_H / 2.0 < offset;
        let below = top + ROW_H / 2.0 > offset + view_h;
        if (above || below) && head_jump(ui, ctx, output.inner_rect, above) {
            tab.history.scroll_to = Some(head.clone());
        }
    }
    let hovered = ctx.hovered_lane.get();
    if hovered != ctx.focus_lane {
        ui.data_mut(|d| d.insert_temp(focus_lane_id(), hovered));
        ui.ctx().request_repaint();
    }
    let x = output.inner_rect.left() + GRAPH_LEFT + ctx.graph_w;
    let bar = Rect::from_x_y_ranges(x - 4.0..=x + 4.0, output.inner_rect.y_range());
    resize_bar(ui, ctx.p, repo.graph.max_lanes, ctx.graph_w, bar);
    if commits.is_empty() {
        let rest = Rect::from_min_max(
            egui::pos2(output.inner_rect.left(), output.inner_rect.top() + wip_rows as f32 * ROW_H),
            output.inner_rect.max,
        );
        ui.painter().text(
            rest.center(),
            Align2::CENTER_CENTER,
            "No commits yet — stage files and commit to create the first one",
            FontId::proportional(13.0),
            ctx.p.text_muted,
        );
    }
}

/// Floating pill at the top (`above`) or bottom of the timeline while HEAD's commit is scrolled out of view:
/// ‹↑› HEAD · branch. Returns whether it was clicked (the caller scrolls HEAD into view).
fn head_jump(ui: &mut Ui, ctx: &RowCtx<'_>, view: Rect, above: bool) -> bool {
    let p = ctx.p;
    let branch = ctx.repo.head.as_ref().and_then(|h| h.branch_name());
    let arrow = if above { icon::ARROW_UP } else { icon::ARROW_DOWN };
    let text = match branch {
        Some(branch) => format!("{arrow}  HEAD · {branch}"),
        None => format!("{arrow}  HEAD (detached)"),
    };
    let text_color = if p.dark { p.bg_graph } else { Color32::WHITE };
    let mut job = egui::text::LayoutJob::simple_singleline(text, FontId::proportional(12.0), text_color);
    job.wrap = egui::text::TextWrapping::truncate_at_width((view.width() - 80.0).max(60.0));
    let galley = ui.painter().layout_job(job);
    let size = galley.size() + egui::vec2(24.0, 10.0);
    let center_x = view.center().x;
    let y = if above { view.top() + 10.0 } else { view.bottom() - 10.0 - size.y };
    let rect = Rect::from_min_size(egui::pos2(center_x - size.x / 2.0, y), size);
    let resp = ui.interact(rect, egui::Id::new("graph_head_jump"), Sense::click());
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Scroll to HEAD"));
    let painter = ui.painter();
    let fill = if resp.hovered() { p.accent } else { p.accent.gamma_multiply(0.9) };
    painter.rect_filled(
        rect.translate(egui::vec2(0.0, 2.0)),
        size.y / 2.0,
        Color32::BLACK.gamma_multiply(0.2),
    );
    painter.rect_filled(rect, size.y / 2.0, fill);
    painter.galley(rect.center() - galley.size() / 2.0, galley, text_color);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text("Scroll to the checked-out commit").clicked()
}

/// The band: a capsule that starts round around the node and runs right through the message, author and age, in
/// a translucent tint of the lane colour, so each row reads as belonging to its node. Hover, search matches and the
/// selection deepen the tint; the selection also gets a lane-coloured outline.
fn band_rect(lay: &RowLayout, node: Pos2) -> Rect {
    Rect::from_min_max(
        egui::pos2(node.x - BAND_R, node.y - BAND_R),
        egui::pos2(lay.graph.right(), node.y + BAND_R),
    )
}

/// Round around the node, square where it meets the resize bar.
fn band_radius() -> egui::CornerRadius {
    egui::CornerRadius {
        nw: BAND_R as u8,
        sw: BAND_R as u8,
        ne: 0,
        se: 0,
    }
}

/// Neutral background of the text part of a row (right of the resize bar): `hover` (0..1, animated) and selection.
fn paint_text_card(painter: &egui::Painter, p: &Palette, lay: &RowLayout, node_y: f32, hover: f32, selected: bool) {
    let rect = Rect::from_min_max(
        egui::pos2(lay.graph.right() + 2.0, node_y - BAND_R),
        egui::pos2(lay.card.right(), node_y + BAND_R),
    );
    if selected {
        painter.rect_filled(rect, 8, p.bg_selected);
    } else if hover > 0.0 {
        painter.rect_filled(rect, 8, p.bg_hover.gamma_multiply(hover));
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_band(
    painter: &egui::Painter,
    p: &Palette,
    lay: &RowLayout,
    node: Pos2,
    lane: Color32,
    hover: f32,
    selected: bool,
    matched: bool,
    dim: f32,
) {
    let rect = band_rect(lay, node);
    let base = if p.dark { 0.12 } else { 0.10 };
    let lit = if selected || matched { 1.0 } else { hover };
    let alpha = (base + lit * base * 1.6) * dim;
    painter.rect_filled(rect, band_radius(), lane.gamma_multiply(alpha));
    if selected {
        painter.rect_stroke(
            rect,
            band_radius(),
            Stroke::new(1.0, lane.gamma_multiply(0.7)),
            egui::StrokeKind::Inside,
        );
    } else if matched {
        painter.rect_stroke(
            rect,
            band_radius(),
            Stroke::new(1.0, p.accent.gamma_multiply(0.7)),
            egui::StrokeKind::Inside,
        );
    }
}

/// Unique paths' change kinds across staged + unstaged (a file in both lists counts once).
fn wip_counts(repo: &RepoData) -> ChangeCounts {
    let mut by_path: HashMap<&Path, DeltaChange> = HashMap::new();
    for d in repo.files.unstaged.iter().chain(&repo.files.staged) {
        by_path.entry(d.path.as_path()).or_insert(d.change);
    }
    ChangeCounts::from_changes(by_path.into_values())
}

/// Dashed circle (the WIP node).
fn dashed_circle(painter: &egui::Painter, center: Pos2, radius: f32, stroke: Stroke) {
    let ring: Vec<Pos2> = (0..=40)
        .map(|i| {
            let a = i as f32 / 40.0 * std::f32::consts::TAU;
            center + radius * egui::vec2(a.cos(), a.sin())
        })
        .collect();
    painter.extend(Shape::dashed_line(&ring, stroke, 3.0, 2.5));
}

/// Dashed outline of a band (the WIP row): round on the left (radius `left`), `right` on the right.
fn dashed_band(painter: &egui::Painter, rect: Rect, left: f32, right: f32, color: Color32) {
    let mut points = Vec::new();
    let corners = [
        (rect.right_top() + egui::vec2(-right, right), -90.0_f32, right),
        (rect.right_bottom() + egui::vec2(-right, -right), 0.0, right),
        (rect.left_bottom() + egui::vec2(left, -left), 90.0, left),
        (rect.left_top() + egui::vec2(left, left), 180.0, left),
    ];
    for (c, start, r) in corners {
        for i in 0..=6 {
            let a = (start + i as f32 * 15.0).to_radians();
            points.push(c + r * egui::vec2(a.cos(), a.sin()));
        }
    }
    if let Some(first) = points.first().copied() {
        points.push(first);
    }
    painter.extend(Shape::dashed_line(&points, Stroke::new(1.0, color), 5.0, 4.0));
}

fn wip_row(ui: &mut Ui, ctx: &RowCtx<'_>, summary: &str, selected: bool) -> egui::Response {
    let p = ctx.p;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), Sense::click());
    plain_cursor(&resp);
    let accessible = format!("// WIP {}", if summary.is_empty() { "Work in progress" } else { summary });
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, selected, &accessible));
    let lay = RowLayout::new(rect, ctx.graph_w);
    let graph_row = ctx.repo.graph.wip.clone().unwrap_or_default();
    let color = p.lane(graph_row.lane);
    let painter = ui.painter();
    let graph_painter = painter.with_clip_rect(lay.graph);
    for e in &graph_row.edges {
        paint_edge(&graph_painter, p, &lay, rect.top(), e, ctx.focus_lane);
    }
    let center = egui::pos2(lay.lane_x(graph_row.lane), rect.center().y);
    let hover = ui.ctx().animate_bool_with_time(resp.id.with("hover"), resp.hovered(), 0.12);
    if resp.hovered() {
        ctx.hovered_lane.set(Some(graph_row.lane));
    }
    paint_text_card(painter, p, &lay, center.y, hover, selected);
    let band = band_rect(&lay, center);
    let tint = if selected { 0.18 } else { 0.05 + hover * 0.08 };
    painter.rect_filled(band, band_radius(), color.gamma_multiply(tint));
    dashed_band(painter, band, BAND_R, 0.0, color.gamma_multiply(if selected { 0.9 } else { 0.55 }));
    graph_painter.circle_filled(center, 9.0, p.bg_graph);
    dashed_circle(&graph_painter, center, 9.0, Stroke::new(1.5, color));
    graph_painter.text(
        center,
        Align2::CENTER_CENTER,
        icon::PENCIL_SIMPLE,
        FontId::proportional(10.0),
        color,
    );

    // Line 1: the draft summary, or "Uncommitted changes".
    let content = painter.with_clip_rect(lay.content);
    let left = lay.content.left() + 4.0;
    let (text, text_color) = if summary.is_empty() {
        ("Uncommitted changes", p.text)
    } else {
        (summary, p.text)
    };
    paint_truncated(
        &content,
        egui::pos2(left, lay.line1),
        text,
        FontId::proportional(13.5),
        text_color,
        lay.content.width() - 8.0,
    );

    // Line 2: change counts and the branch they will land on.
    let counts = wip_counts(ctx.repo);
    let mut x = left;
    for (n, glyph, c) in [
        (counts.modified, icon::PENCIL_SIMPLE, p.orange),
        (counts.added, icon::PLUS, p.green),
        (counts.deleted, icon::MINUS, p.red),
    ] {
        if n > 0 {
            let r = content.text(
                egui::pos2(x, lay.line2),
                Align2::LEFT_CENTER,
                format!("{glyph} {n}"),
                FontId::proportional(11.5),
                c,
            );
            x = r.right() + 10.0;
        }
    }
    if let Some(branch) = ctx.repo.head.as_ref().and_then(|h| h.branch_name()) {
        content.text(
            egui::pos2(x, lay.line2),
            Align2::LEFT_CENTER,
            format!("on {}  {branch}", icon::GIT_BRANCH),
            FontId::proportional(11.5),
            p.text_faint,
        );
    }

    // Right: call to action.
    let cta_color = if selected || resp.hovered() { p.accent } else { p.text_faint };
    painter.text(
        egui::pos2(lay.meta.right(), rect.center().y),
        Align2::RIGHT_CENTER,
        format!("Review {}", icon::ARROW_RIGHT),
        FontId::proportional(12.0),
        cta_color,
    );
    resp
}

fn shallow_row(ui: &mut Ui, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), Sense::hover());
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        format!("{}  History is truncated (shallow clone)", icon::SCISSORS),
        FontId::proportional(12.0),
        p.text_faint,
    );
}

/// Commit node: a filled dot ringed with the background (merge commits: a smaller hollow ring; HEAD: an extra halo).
fn paint_node(painter: &egui::Painter, p: &Palette, center: Pos2, color: Color32, merge: bool, head: bool, selected: bool) {
    if head {
        painter.circle_filled(center, NODE_R + 5.0, color.gamma_multiply(0.22));
        painter.circle_stroke(center, NODE_R + 5.0, Stroke::new(1.5, color));
    }
    let r = if selected { NODE_R + 1.0 } else { NODE_R };
    if merge {
        painter.circle_filled(center, r, p.bg_graph);
        painter.circle_stroke(center, r - 1.0, Stroke::new(2.0, color));
    } else {
        painter.circle_filled(center, r + 2.0, p.bg_graph);
        painter.circle_filled(center, r, color);
    }
}

fn commit_row(
    ui: &mut Ui,
    ctx: &RowCtx<'_>,
    commit: &Commit,
    row: Option<&GraphRow>,
    selected: bool,
    out: &mut Vec<Intent>,
) -> egui::Response {
    let p = ctx.p;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), Sense::click());
    plain_cursor(&resp);
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, selected, &commit.summary));
    let lay = RowLayout::new(rect, ctx.graph_w);
    let lane = row.map_or(0, |r| r.lane);
    let color = p.lane(lane);
    let matched = ctx.search.matches(commit);

    let painter = ui.painter().clone();
    let graph_painter = painter.with_clip_rect(lay.graph);
    if let Some(row) = row {
        for e in &row.edges {
            paint_edge(&graph_painter, p, &lay, rect.top(), e, ctx.focus_lane);
        }
    }
    let node = egui::pos2(lay.lane_x(lane), rect.center().y);
    let hovered = ui.rect_contains_pointer(rect);
    if hovered {
        ctx.hovered_lane.set(Some(lane));
    }
    let hover = ui.ctx().animate_bool_with_time(resp.id.with("hover"), hovered, 0.12);
    paint_text_card(&painter, p, &lay, node.y, hover, selected);
    let dim = if selected { 1.0 } else { lane_alpha(ctx.focus_lane, lane) };
    paint_band(&painter, p, &lay, node, color, hover, selected, matched, dim);
    let head = ctx.repo.head_target() == Some(&commit.id);
    paint_node(&graph_painter, p, node, color, commit.is_merge(), head, selected);

    // Line 1: the HEAD tag, ref chips, then the summary.
    let refs = ctx.repo.refs_by_commit.get(&commit.id).map_or(&[][..], Vec::as_slice);
    let pills = build_pills(&commit.id, refs, ctx.repo.head.as_ref(), &ctx.menu.remotes, ctx.opts);
    let mut msg_left = lay.content.left() + 4.0;
    // A detached HEAD already has its own pill.
    if head && !pills.iter().any(|pill| pill.kind == PillKind::DetachedHead) {
        msg_left = head_tag(ui, ctx, &lay, msg_left, resp.id) + 4.0;
    }
    if !pills.is_empty() {
        msg_left = paint_pills(ui, ctx, &lay, msg_left, commit, &pills, color, out) + 8.0;
    }
    if commit.lossy {
        let r = painter.text(
            egui::pos2(msg_left, lay.line1),
            Align2::LEFT_CENTER,
            icon::WARNING,
            FontId::proportional(12.0),
            p.orange,
        );
        ui.interact(r, resp.id.with("lossy"), Sense::hover())
            .on_hover_text("Commit text is not valid UTF-8");
        msg_left = r.right() + 4.0;
    }
    let text_color = if commit.is_merge() && !selected { p.text_muted } else { p.text };
    paint_truncated(
        &painter.with_clip_rect(lay.content),
        egui::pos2(msg_left, lay.line1),
        &commit.summary,
        FontId::proportional(13.5),
        text_color,
        lay.content.right() - msg_left,
    );

    // Line 2: avatar, author, description preview.
    let line2 = painter.with_clip_rect(lay.content);
    let left = lay.content.left() + 4.0;
    paint_avatar(&line2, p, egui::pos2(left + 7.0, lay.line2), 7.0, &commit.author_name, None);
    let author = paint_truncated(
        &line2,
        egui::pos2(left + 20.0, lay.line2),
        &commit.author_name,
        FontId::proportional(12.0),
        color,
        (lay.content.width() * 0.4).max(60.0),
    );
    if !commit.description.is_empty() {
        let preview = commit.description.lines().find(|l| !l.trim().is_empty()).unwrap_or_default();
        paint_truncated(
            &line2,
            egui::pos2(author.right() + 10.0, lay.line2),
            &format!("—  {preview}"),
            FontId::proportional(11.5),
            p.text_faint,
            lay.content.right() - author.right() - 14.0,
        );
    }

    // Right: quick actions while hovered, else the age (exact time on hover) and short SHA.
    if hovered && quick_actions(ui, ctx, &lay, commit, resp.id, out) {
        resp.context_menu(|ui| commit_menu(ui, ctx, commit, out));
        return resp;
    }
    let age = painter.text(
        egui::pos2(lay.meta.right(), lay.line1),
        Align2::RIGHT_CENTER,
        crate::ui::format_relative(commit.author_time),
        FontId::proportional(12.0),
        p.text_muted,
    );
    ui.interact(age, resp.id.with("age"), Sense::hover())
        .on_hover_text(crate::ui::format_time(commit.author_time));
    painter.text(
        egui::pos2(lay.meta.right(), lay.line2),
        Align2::RIGHT_CENTER,
        commit.short_id(),
        FontId::monospace(11.0),
        p.text_faint,
    );

    resp.context_menu(|ui| commit_menu(ui, ctx, commit, out));
    resp
}

/// Icon buttons in the right-hand column of a hovered commit: check out, new branch, new tag, copy SHA ("Copied"
/// replaces them for a moment). Returns whether the column was drawn (the age and SHA are then skipped).
fn quick_actions(ui: &mut Ui, ctx: &RowCtx<'_>, lay: &RowLayout, commit: &Commit, row: egui::Id, out: &mut Vec<Intent>) -> bool {
    let p = ctx.p;
    let copied_id = egui::Id::new(("graph_copied", &commit.id));
    let now = ui.input(|i| i.time);
    if let Some(at) = ui.data(|d| d.get_temp::<f64>(copied_id))
        && now - at < 1.2
    {
        ui.painter().text(
            egui::pos2(lay.meta.right(), lay.meta.center().y),
            Align2::RIGHT_CENTER,
            format!("{}  Copied", icon::CHECK),
            FontId::proportional(12.0),
            p.green,
        );
        ui.ctx().request_repaint();
        return true;
    }
    let checked_out = ctx.repo.head_target() == Some(&commit.id);
    let mut buttons: Vec<(&str, &str)> = Vec::new();
    if !checked_out {
        buttons.push((icon::ARROW_SQUARE_IN, "Check out (detached HEAD)"));
    }
    buttons.extend([
        (icon::GIT_BRANCH, "New branch from this commit"),
        (icon::TAG, "New tag on this commit"),
        (icon::COPY, "Copy SHA"),
    ]);
    let size = egui::vec2(24.0, 24.0);
    let mut x = lay.meta.right() - buttons.len() as f32 * size.x;
    for (i, (glyph, label)) in buttons.iter().enumerate() {
        let r = Rect::from_min_size(egui::pos2(x, lay.meta.center().y - size.y / 2.0), size);
        let resp = ui.interact(r, row.with(("quick", i)), Sense::click());
        resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, *label));
        if resp.hovered() {
            ui.painter().rect_filled(r, 6, p.bg_selected);
        }
        let color = if resp.hovered() { p.accent } else { p.text_muted };
        ui.painter()
            .text(r.center(), Align2::CENTER_CENTER, *glyph, FontId::proportional(14.0), color);
        if resp.on_hover_text(*label).clicked() {
            match *glyph {
                icon::ARROW_SQUARE_IN => out.push(Intent::CheckoutCommit(commit.id.clone())),
                icon::GIT_BRANCH => out.push(Intent::CreateBranch {
                    from: Some(commit.id.clone()),
                    label: None,
                }),
                icon::TAG => out.push(Intent::CreateTag(Some(commit.id.clone()))),
                _ => {
                    ui.ctx().copy_text(commit.id.to_hex());
                    ui.data_mut(|d| d.insert_temp(copied_id, now));
                }
            }
        }
        x += size.x;
    }
    true
}

fn commit_menu(ui: &mut Ui, ctx: &RowCtx<'_>, commit: &Commit, out: &mut Vec<Intent>) {
    let detached_here = matches!(ctx.repo.head.as_ref(), Some(gityas_core::models::Head::Detached { target }) if *target == commit.id);
    if menus::item(ui, "Checkout this commit", detached_here.then_some("Already checked out")) {
        out.push(Intent::CheckoutCommit(commit.id.clone()));
    }
    if menus::item(ui, "Create branch here…", None) {
        out.push(Intent::CreateBranch {
            from: Some(commit.id.clone()),
            label: None,
        });
    }
    if menus::item(ui, "Create tag here…", None) {
        out.push(Intent::CreateTag(Some(commit.id.clone())));
    }
    let subject = ctx.menu.current.as_deref().unwrap_or("HEAD");
    let label = format!("Reset {subject} to this commit");
    let reset_disabled = if ctx.menu.unborn || ctx.repo.head.is_none() {
        Some("Nothing is checked out")
    } else if ctx.repo.head_target() == Some(&commit.id) {
        Some("Already at this commit")
    } else {
        None
    };
    match reset_disabled {
        Some(reason) => {
            menus::item(ui, &label, Some(reason));
        }
        None => {
            ui.menu_button(label, |ui| {
                for (text, mode) in [
                    ("Soft - keep all changes", ResetMode::Soft),
                    ("Hard - discard all changes…", ResetMode::Hard),
                ] {
                    if menus::item(ui, text, None) {
                        out.push(Intent::Reset {
                            commit: commit.id.clone(),
                            mode,
                        });
                    }
                }
            });
        }
    }
    ui.separator();
    if menus::item(ui, "Copy SHA", None) {
        ui.ctx().copy_text(commit.id.to_hex());
    }
    if menus::item(ui, "Copy short SHA", None) {
        ui.ctx().copy_text(commit.short_id());
    }
    if menus::item(ui, "Copy message", None) {
        ui.ctx().copy_text(commit.summary.clone());
    }
}

/// Looks up the sidebar model of a branch pill.
fn branch_of<'a>(repo: &'a RepoData, refname: &str) -> Option<&'a Branch> {
    repo.local_branches
        .iter()
        .chain(&repo.remote_branches)
        .find(|b| b.refname == refname)
}

/// The menu for one pill (same as the sidebar's).
fn pill_menu(ui: &mut Ui, ctx: &RowCtx<'_>, pill: &Pill, out: &mut Vec<Intent>) {
    let Some(refname) = &pill.refname else {
        ui.label("HEAD is detached at this commit");
        return;
    };
    match pill.kind {
        PillKind::Local | PillKind::Remote => match branch_of(ctx.repo, refname) {
            Some(b) if b.is_local() => menus::local_branch(ui, b, &ctx.menu, out),
            Some(b) => menus::remote_branch(ui, b, &ctx.menu, out),
            None => {
                ui.label("Loading branches…");
            }
        },
        PillKind::Tag => {
            let tags = &ctx.repo.commits.tags;
            let local = tags.iter().position(|t| t.name == pill.label);
            let on_remotes = match ctx.remote_tags {
                RemoteTags::Loaded(map) => {
                    let mut r: Vec<String> = map
                        .iter()
                        .filter(|(_, names)| names.contains(&pill.label))
                        .map(|(r, _)| r.clone())
                        .collect();
                    r.sort();
                    r
                }
                _ => Vec::new(),
            };
            let row = TagRow {
                name: pill.label.clone(),
                local,
                on_remotes,
            };
            menus::tag(ui, &row, local.and_then(|i| tags.get(i)), &ctx.menu, ctx.remote_tags, out);
        }
        PillKind::DetachedHead => {}
    }
}

fn pill_text(pill: &Pill) -> (String, String) {
    let label = if pill.head && pill.kind == PillKind::Local {
        format!("{}  {}", icon::CHECK, pill.label)
    } else {
        pill.label.clone()
    };
    let glyphs = match pill.kind {
        PillKind::Local if pill.remote => format!("{} {}", icon::DESKTOP, icon::CLOUD),
        PillKind::Local => icon::DESKTOP.to_owned(),
        PillKind::Remote => icon::CLOUD.to_owned(),
        PillKind::Tag => icon::TAG.to_owned(),
        PillKind::DetachedHead => icon::CROSSHAIR.to_owned(),
    };
    (label, glyphs)
}

/// Chip colours: HEAD's branch is solid, other branches tinted, tags outlined only.
fn chip_style(p: &Palette, pill: &Pill, lane: Color32) -> (Color32, Stroke, Color32) {
    let color = if pill.kind == PillKind::DetachedHead { p.orange } else { lane };
    if pill.head || pill.kind == PillKind::DetachedHead {
        // Pastel lanes (dark theme) need dark text; the darkened light-theme lanes take white.
        let text = if p.dark { p.bg_graph } else { Color32::WHITE };
        (color, Stroke::NONE, text)
    } else if pill.kind == PillKind::Tag {
        (Color32::TRANSPARENT, Stroke::new(1.0, color.gamma_multiply(0.7)), color)
    } else {
        (
            color.gamma_multiply(if p.dark { 0.16 } else { 0.12 }),
            Stroke::new(1.0, color.gamma_multiply(0.35)),
            color,
        )
    }
}

fn chip_glyph(pill: &Pill) -> &'static str {
    match pill.kind {
        PillKind::Local if pill.head => icon::CHECK_CIRCLE,
        PillKind::Local if pill.remote => icon::CLOUD_CHECK,
        PillKind::Local => icon::GIT_BRANCH,
        PillKind::Remote => icon::CLOUD,
        PillKind::Tag => icon::TAG,
        PillKind::DetachedHead => icon::CROSSHAIR,
    }
}

/// The accent `HEAD` tag at `left` on line 1 of the checked-out commit (whose branch pill follows). Returns its
/// right edge.
fn head_tag(ui: &mut Ui, ctx: &RowCtx<'_>, lay: &RowLayout, left: f32, row: egui::Id) -> f32 {
    let p = ctx.p;
    let painter = ui.painter().with_clip_rect(lay.content);
    let text_color = if p.dark { p.bg_graph } else { Color32::WHITE };
    let galley = painter.layout_no_wrap(format!("{}  HEAD", icon::CROSSHAIR), FontId::proportional(11.0), text_color);
    let rect = Rect::from_min_size(
        egui::pos2(left, lay.line1 - CHIP_H / 2.0),
        egui::vec2(galley.size().x + 16.0, CHIP_H),
    );
    painter.rect_filled(rect, CHIP_H / 2.0, p.accent);
    painter.galley(rect.center() - galley.size() / 2.0, galley, text_color);
    let tooltip = match ctx.repo.head.as_ref().and_then(|h| h.branch_name()) {
        Some(branch) => format!("HEAD: {branch} is checked out here"),
        None => "HEAD".to_owned(),
    };
    ui.interact(rect, row.with("head_tag"), Sense::hover()).on_hover_text(tooltip);
    rect.right()
}

/// Paints up to [`MAX_CHIPS`] ref chips on line 1 from `left` (plus a `+N` chip with a popup listing every ref).
/// Returns the right edge drawn.
#[allow(clippy::too_many_arguments)]
fn paint_pills(
    ui: &mut Ui,
    ctx: &RowCtx<'_>,
    lay: &RowLayout,
    left: f32,
    commit: &Commit,
    pills: &[Pill],
    color: Color32,
    out: &mut Vec<Intent>,
) -> f32 {
    let p = ctx.p;
    // Chips may take at most half the content width; the summary keeps the rest.
    let limit = lay.content.left() + lay.content.width() * 0.5;
    let painter = ui.painter().with_clip_rect(lay.content);
    let font = FontId::proportional(11.5);
    let tooltip = pills_tooltip(pills);
    let mut x = left;
    let shown = pills.len().min(MAX_CHIPS);
    for (i, pill) in pills.iter().take(shown).enumerate() {
        let (fill, stroke, text_color) = chip_style(p, pill, color);
        let glyph = painter.layout_no_wrap(chip_glyph(pill).to_owned(), FontId::proportional(11.0), text_color);
        let room = (limit - x - glyph.size().x - 20.0).max(24.0);
        let mut job = egui::text::LayoutJob::simple_singleline(pill.label.clone(), font.clone(), text_color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(room);
        let text = painter.layout_job(job);
        let width = 8.0 + glyph.size().x + 4.0 + text.size().x + 9.0;
        let rect = Rect::from_min_size(egui::pos2(x, lay.line1 - CHIP_H / 2.0), egui::vec2(width, CHIP_H));
        painter.rect_filled(rect, CHIP_H / 2.0, fill);
        if stroke != Stroke::NONE {
            painter.rect_stroke(rect, CHIP_H / 2.0, stroke, egui::StrokeKind::Inside);
        }
        painter.galley(
            egui::pos2(rect.left() + 8.0, rect.center().y - glyph.size().y / 2.0),
            glyph.clone(),
            text_color,
        );
        painter.galley(
            egui::pos2(rect.left() + 8.0 + glyph.size().x + 4.0, rect.center().y - text.size().y / 2.0),
            text,
            text_color,
        );

        let resp = ui.interact(rect, egui::Id::new(("pill", &commit.id, i)), Sense::click());
        resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, format!("{} pill", pill.label)));
        if resp.double_clicked()
            && pill.is_branch()
            && !pill.head
            && let Some(refname) = &pill.refname
        {
            out.push(Intent::CheckoutBranch {
                refname: refname.clone(),
                label: pill.label.clone(),
            });
        }
        resp.context_menu(|ui| pill_menu(ui, ctx, pill, out));
        resp.on_hover_text(&tooltip);
        x = rect.right() + 4.0;
    }

    let overflow = pills.len() - shown;
    if overflow > 0 {
        let label = format!("+{overflow}");
        let galley = painter.layout_no_wrap(label, FontId::proportional(11.0), p.text_muted);
        let badge = Rect::from_min_size(egui::pos2(x, lay.line1 - CHIP_H / 2.0), egui::vec2(galley.size().x + 14.0, CHIP_H));
        let resp = ui.interact(badge, egui::Id::new(("pill_more", &commit.id)), Sense::click());
        resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, format!("+{overflow} more refs")));
        painter.rect_filled(badge, CHIP_H / 2.0, if resp.hovered() { p.bg_selected } else { p.bg_hover });
        painter.galley(badge.center() - galley.size() / 2.0, galley, p.text_muted);
        egui::Popup::menu(&resp).show(|ui| {
            for pill in pills {
                let (label, glyphs) = pill_text(pill);
                if pill.kind == PillKind::DetachedHead {
                    ui.label(format!("{glyphs} {label}"));
                } else {
                    ui.menu_button(format!("{glyphs} {label}"), |ui| pill_menu(ui, ctx, pill, out));
                }
            }
        });
        resp.on_hover_text(&tooltip);
        x = badge.right() + 4.0;
    }
    x - 4.0
}

fn paint_edge(painter: &egui::Painter, p: &Palette, lay: &RowLayout, row_top: f32, e: &Edge, focus: Option<usize>) {
    let p0 = egui::pos2(lay.lane_x(e.from.lane), row_top + e.from.y * ROW_H);
    let p1 = egui::pos2(lay.lane_x(e.to.lane), row_top + e.to.y * ROW_H);
    let width = if focus == Some(e.color_lane) { 2.5 } else { 2.0 };
    let stroke = Stroke::new(width, p.lane(e.color_lane).gamma_multiply(0.85 * lane_alpha(focus, e.color_lane)));
    if e.dashed {
        painter.extend(Shape::dashed_line(&[p0, p1], stroke, 4.0, 3.0));
    } else if e.from.lane == e.to.lane {
        painter.line_segment([p0, p1], stroke);
    } else {
        let mid = (p0.y + p1.y) / 2.0;
        painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
            [p0, egui::pos2(p0.x, mid), egui::pos2(p1.x, mid), p1],
            false,
            Color32::TRANSPARENT,
            stroke,
        ));
    }
}
