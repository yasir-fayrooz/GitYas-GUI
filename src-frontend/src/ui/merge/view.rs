//! The merge editor's egui screens: header (with the conflict progress strip), the editable result on the left and
//! the two read-only sources stacked on the right, the status bar, the action card and the loading / error /
//! "no longer conflicted" states. All merge logic lives in [`super::model`].

use std::path::{Path, PathBuf};

use egui::text::LayoutJob;
use egui::{
    Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Frame, Id, Key, KeyboardShortcut, Layout, Margin, Modifiers, Pos2, Rect,
    RichText, ScrollArea, Sense, Shape, Stroke, StrokeKind, Ui, UiBuilder, Vec2, WidgetInfo, WidgetType, pos2,
};
use egui_phosphor::regular as icon;
use gityas_core::models::{MergeConflict, RepoStatus};

use super::dock::{self, Node, Slot};
use super::labels::{self, Labels};
use super::model::{ChunkState, MergeModel, Pane, Side, Tri};
use super::rows::{self, Fill, ROW_H, RowStyle, SPINE_COL, Segment};
use super::{Request, State, TASK_ACTION, TASK_SAVE};
use crate::app::GitYasApp;
use crate::syntax::{HighlightedText, Highlights};
use crate::theme::{self, Palette};
use crate::ui::{cmd, cmd_shift, icon_button, solid_button};

const PANES: [Pane; 3] = [Pane::Ours, Pane::Theirs, Pane::Output];

/// Scroll state of one pane, for programmatic scrolling and scroll sync.
#[derive(Debug, Default, Clone, Copy)]
struct PaneScroll {
    /// Offset to apply on the next frame.
    set: Option<f32>,
    /// Offset seen last frame (a difference = the user scrolled this pane).
    last: f32,
    viewport_h: f32,
    /// Height of the scrolled content (the offset can go up to `content_h - viewport_h`).
    content_h: f32,
    row_h: f32,
}

/// Where a revealed chunk sits in a pane (fraction of the viewport height from the top). Scroll-sync anchors
/// use the same position, so panes revealed together stay in step when the user scrolls on from there.
const ANCHOR_FRACTION: f32 = 1.0 / 3.0;

/// Syntax highlights of the read-only sides, computed with the conflict (off the UI thread).
#[derive(Debug, Default)]
pub(super) struct SideSyntax {
    path: PathBuf,
    ours: HighlightedText,
    theirs: HighlightedText,
}

impl SideSyntax {
    pub(super) fn compute(path: &Path, conflict: &MergeConflict) -> Self {
        Self {
            path: path.to_owned(),
            ours: HighlightedText::new(path, &conflict.ours),
            theirs: HighlightedText::new(path, &conflict.theirs),
        }
    }

    fn side(&self, side: Side) -> &HighlightedText {
        match side {
            Side::Ours => &self.ours,
            Side::Theirs => &self.theirs,
        }
    }
}

/// Syntax highlights of the editable output, recomputed whenever its text changes.
#[derive(Debug, Default)]
struct OutputSyntax {
    /// The text `highlights` belong to.
    text: Option<String>,
    highlights: Highlights,
}

impl OutputSyntax {
    fn highlights(&mut self, path: &Path, text: &str) -> &Highlights {
        if self.text.as_deref() != Some(text) {
            self.highlights = Highlights::compute(path, text);
            self.text = Some(text.to_owned());
        }
        &self.highlights
    }
}

/// A loaded text conflict.
pub(super) struct Editor {
    pub(super) model: MergeModel,
    scroll: [PaneScroll; 3],
    /// Scroll every pane to the current chunk on the next frame.
    reveal: bool,
    syntax: SideSyntax,
    output_syntax: OutputSyntax,
    /// The pane layout drawn last frame (a different arrangement re-reveals the current chunk).
    layout: Option<Node>,
}

impl Editor {
    pub(super) fn new(model: MergeModel, syntax: SideSyntax) -> Self {
        Self {
            model,
            scroll: [PaneScroll::default(); 3],
            reveal: true,
            syntax,
            output_syntax: OutputSyntax::default(),
            layout: None,
        }
    }

    fn scroll_mut(&mut self, pane: Pane) -> &mut PaneScroll {
        &mut self.scroll[pane_index(pane)]
    }

    /// Queues scrolling so the current chunk sits about a third down each pane.
    fn reveal_current(&mut self) {
        let i = self.model.current();
        for pane in PANES {
            let Some(range) = self.model.chunk_range(pane, i) else { continue };
            let s = self.scroll_mut(pane);
            let row_h = if s.row_h > 0.0 { s.row_h } else { ROW_H };
            s.set = Some((range.start as f32 * row_h - s.viewport_h * ANCHOR_FRACTION).max(0.0));
        }
    }

    /// Scroll offsets that line up across the panes, one list per pane (indexed by [`pane_index`]): the top, each
    /// chunk at its reveal position, and the bottom. See [`joint_anchors`].
    fn anchors(&self) -> [Vec<f32>; 3] {
        let chunks: Vec<[Option<f32>; 3]> = (0..self.model.chunk_count())
            .map(|i| {
                PANES.map(|pane| {
                    let s = self.scroll[pane_index(pane)];
                    let row_h = if s.row_h > 0.0 { s.row_h } else { ROW_H };
                    let range = self.model.chunk_range(pane, i)?;
                    Some(range.start as f32 * row_h - s.viewport_h * ANCHOR_FRACTION)
                })
            })
            .collect();
        let max = PANES.map(|pane| {
            let s = self.scroll[pane_index(pane)];
            (s.content_h - s.viewport_h).max(0.0)
        });
        joint_anchors(&chunks, max)
    }

    /// The user scrolled `from` to `offset`: scroll the other panes to the same relative position between the
    /// same two anchors (as the Tauri editor did), so every pane moves smoothly and they meet at each chunk.
    fn sync_from(&mut self, from: Pane, offset: f32) {
        let anchors = self.anchors();
        let (segment, progress) = anchor_position(&anchors[pane_index(from)], offset);
        for to in PANES {
            if to == from {
                continue;
            }
            let y = anchor_offset(&anchors[pane_index(to)], segment, progress);
            self.scroll_mut(to).set = Some(y);
        }
    }
}

/// Builds the panes' anchor lists from each chunk's unclamped reveal offset per pane and each pane's max offset.
/// A chunk only becomes an anchor when it lies strictly inside the scroll range of every scrollable pane, after
/// the previous anchor: a chunk that clamps to the top or bottom of one pane but not another would otherwise
/// give that pane a zero-length segment, and the panes would stall and then jump past it. This keeps every
/// mapping between two scrollable panes strictly increasing and continuous, whichever pane is scrolled.
fn joint_anchors(chunks: &[[Option<f32>; 3]], max: [f32; 3]) -> [Vec<f32>; 3] {
    const MIN_GAP: f32 = 1.0;
    let mut anchors: [Vec<f32>; 3] = std::array::from_fn(|_| vec![0.0]);
    for chunk in chunks {
        let Some(ys) = chunk[0].zip(chunk[1]).zip(chunk[2]).map(|((a, b), c)| [a, b, c]) else {
            continue;
        };
        let inside = (0..3).all(|k| {
            let prev = anchors[k].last().copied().unwrap_or(0.0);
            max[k] < MIN_GAP || (ys[k] >= prev + MIN_GAP && ys[k] <= max[k] - MIN_GAP)
        });
        if inside {
            for k in 0..3 {
                anchors[k].push(ys[k].clamp(0.0, max[k]));
            }
        }
    }
    for k in 0..3 {
        anchors[k].push(max[k]);
    }
    anchors
}

/// The anchor segment `offset` falls in, and how far along it (0..=1).
fn anchor_position(anchors: &[f32], offset: f32) -> (usize, f32) {
    let last = anchors.len().saturating_sub(2);
    let segment = (0..last).find(|&i| offset <= anchors[i + 1]).unwrap_or(last);
    let (start, end) = (anchors[segment], anchors.get(segment + 1).copied().unwrap_or(anchors[segment]));
    let progress = if end > start {
        ((offset - start) / (end - start)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (segment, progress)
}

/// The offset `progress` of the way along anchor segment `segment`.
fn anchor_offset(anchors: &[f32], segment: usize, progress: f32) -> f32 {
    let segment = segment.min(anchors.len().saturating_sub(2));
    let start = anchors[segment];
    let end = anchors.get(segment + 1).copied().unwrap_or(start);
    start + (end - start) * progress
}

fn pane_index(pane: Pane) -> usize {
    match pane {
        Pane::Ours => 0,
        Pane::Theirs => 1,
        Pane::Output => 2,
    }
}

/// Edits requested while drawing the read-only parts (applied before the result is drawn).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    Toggle(usize, Side),
    /// Chunk, side, line within the chunk.
    ToggleLine(usize, Side, usize),
    All(Side, bool),
    Current(usize),
    /// Make a chunk current and scroll every pane to it (progress strip).
    Jump(usize),
    Prev,
    Next,
    Undo,
    Redo,
    Reset,
}

/// Labels for the active repository (ours = HEAD, theirs = the merged-in side).
pub(super) fn labels_for(app: &GitYasApp) -> Labels {
    match app.store.repo.as_ref() {
        Some(r) => labels::side_labels(r.head.as_ref(), &r.status, &r.local_branches, &r.remote_branches),
        None => labels::side_labels(None, &RepoStatus::default(), &[], &[]),
    }
}

pub(super) fn output_id(path: &Path) -> Id {
    Id::new(("merge-output", path))
}

/// Draws the editor for `path` in the given state; returns what the user asked for.
pub(super) fn show(ui: &mut Ui, app: &mut GitYasApp, path: &Path, state: &mut State) -> Option<Request> {
    let p = theme::pal(ui);
    let labels = labels_for(app);
    let saving = app.bridge.is_running(TASK_SAVE);
    let acting = app.bridge.is_running(TASK_ACTION);
    let modal_open = app.confirm.is_some() || ui.ctx().memory(|m| m.top_modal_layer().is_some());
    // Esc closes an open menu (the layout menu) first, not the editor; the menu closes itself while drawing.
    let menu_was_open = egui::Popup::is_any_open(ui.ctx());
    let mut request = None;
    egui::CentralPanel::default().frame(Frame::new().fill(p.bg_graph)).show(ui, |ui| {
        let edits = header(ui, p, path, state, saving, &mut request);
        if let State::Editor(editor) = state {
            for edit in edits {
                apply(editor, edit);
            }
        }
        match state {
            // Stacked: a horizontal row would fill the width and start at the left edge.
            State::Idle | State::Loading => centered(ui, |ui| {
                ui.add(egui::Spinner::new());
                ui.add_space(6.0);
                ui.label(RichText::new("Loading conflict…").color(p.text_muted));
            }),
            State::Failed(message) => {
                let message = message.clone();
                card(ui, p, "Could not load the conflict", |ui| {
                    ui.add(egui::Label::new(RichText::new(&message).color(p.text_muted)).wrap());
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Retry").clicked() {
                            request = Some(Request::Retry);
                        }
                        if ui.button("Close").clicked() {
                            request = Some(Request::Close);
                        }
                    });
                });
            }
            State::Gone => card(ui, p, "No longer in conflict", |ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(format!(
                            "{} is no longer in conflict: it was resolved outside the editor or the merge was aborted.",
                            path.display()
                        ))
                        .color(p.text_muted),
                    )
                    .wrap(),
                );
                ui.add_space(8.0);
                if ui.button("Close").clicked() {
                    request = Some(Request::Close);
                }
            }),
            State::Action(conflict) => action_card(ui, p, path, conflict, &labels, acting, &mut request),
            State::Editor(editor) => editor_body(ui, p, path, editor, &labels, saving),
        }
    });

    if !modal_open {
        // "Free" = no text field has keyboard focus (a focused button doesn't count).
        let typing = ui.ctx().egui_wants_keyboard_input();
        let in_output = ui.ctx().memory(|m| m.focused()) == Some(output_id(path));
        if let State::Editor(editor) = state {
            keyboard(ui, editor, !typing, in_output);
        }
        if !typing && !menu_was_open && ui.ctx().input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            request = Some(Request::Close);
        }
    }
    request
}

fn keyboard(ui: &Ui, editor: &mut Editor, free: bool, in_output: bool) {
    let ctx = ui.ctx();
    if free || in_output {
        let redo = ctx.input_mut(|i| i.consume_shortcut(&cmd_shift(Key::Z)) || i.consume_shortcut(&cmd(Key::Y)));
        let undo = ctx.input_mut(|i| i.consume_shortcut(&cmd(Key::Z)));
        if redo {
            apply(editor, Edit::Redo);
        } else if undo {
            apply(editor, Edit::Undo);
        }
    }
    if free {
        let next = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::J) || i.consume_key(Modifiers::NONE, Key::F7));
        let prev = ctx
            .input_mut(|i| i.consume_key(Modifiers::NONE, Key::K) || i.consume_shortcut(&KeyboardShortcut::new(Modifiers::SHIFT, Key::F7)));
        if next {
            apply(editor, Edit::Next);
        }
        if prev {
            apply(editor, Edit::Prev);
        }
        // O / T: use (or stop using) all of ours / theirs for the current conflict.
        if editor.model.chunk_count() > 0 {
            let current = editor.model.current();
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::O)) {
                apply(editor, Edit::Toggle(current, Side::Ours));
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::T)) {
                apply(editor, Edit::Toggle(current, Side::Theirs));
            }
        }
    }
}

fn apply(editor: &mut Editor, edit: Edit) {
    let model = &mut editor.model;
    let reveal = match edit {
        Edit::Toggle(i, side) => {
            model.toggle(i, side);
            false
        }
        Edit::ToggleLine(i, side, k) => {
            model.toggle_line(i, side, k);
            false
        }
        Edit::All(side, on) => {
            model.set_side_all(side, on);
            false
        }
        Edit::Current(i) => {
            model.set_current(i);
            false
        }
        Edit::Jump(i) => {
            model.set_current(i);
            true
        }
        Edit::Prev => model.prev(),
        Edit::Next => model.next(),
        Edit::Undo => {
            model.undo();
            false
        }
        Edit::Redo => {
            model.redo();
            false
        }
        Edit::Reset => {
            model.reset();
            true
        }
    };
    if reveal {
        editor.reveal_current();
    }
}

fn centered(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.35).max(0.0));
        add(ui);
    });
}

/// Centred card with a title (loading errors, "no longer in conflict", "Action needed").
fn card(ui: &mut Ui, p: &Palette, title: &str, add: impl FnOnce(&mut Ui)) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.25).max(0.0));
        Frame::new()
            .fill(p.bg_panel)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(6)
            .inner_margin(16)
            .show(ui, |ui| {
                ui.set_max_width(560.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(title).heading().strong());
                    ui.add_space(6.0);
                    add(ui);
                });
            });
    });
}

// ---- Chunk status -------------------------------------------------------------------------------------------

/// How a chunk is resolved, as the progress strip and the result's chips show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Unresolved,
    Ours,
    Theirs,
    Both,
    /// Some lines of one or both sides.
    Mixed,
    Edited,
}

fn status(c: &ChunkState) -> Status {
    if c.edited || (!c.kept.is_empty() && !c.sel.is_taken()) {
        return Status::Edited;
    }
    match (c.sel.ours.state(), c.sel.theirs.state()) {
        (Tri::None, Tri::None) => Status::Unresolved,
        (Tri::All, Tri::None) => Status::Ours,
        (Tri::None, Tri::All) => Status::Theirs,
        (Tri::All, Tri::All) => Status::Both,
        _ => Status::Mixed,
    }
}

fn status_text(s: Status) -> &'static str {
    match s {
        Status::Unresolved => "unresolved",
        Status::Ours => "ours",
        Status::Theirs => "theirs",
        Status::Both => "both",
        Status::Mixed => "mixed",
        Status::Edited => "edited",
    }
}

/// The colour (or the two colours, ours first) a status is drawn in.
fn status_colors(p: &Palette, c: &ChunkState) -> (Color32, Option<Color32>) {
    let s = status(c);
    match s {
        Status::Unresolved => (p.orange, None),
        Status::Ours => (side_color(p, Side::Ours), None),
        Status::Theirs => (side_color(p, Side::Theirs), None),
        Status::Edited => (p.green, None),
        Status::Both | Status::Mixed => {
            let dim = |side: Side| {
                let color = side_color(p, side);
                match c.sel.side(side).state() {
                    Tri::All => Some(color),
                    Tri::Some => Some(color.gamma_multiply(0.6)),
                    Tri::None => None,
                }
            };
            match (dim(Side::Ours), dim(Side::Theirs)) {
                (Some(a), Some(b)) => (a, Some(b)),
                (Some(a), None) | (None, Some(a)) => (a, None),
                (None, None) => (p.text_muted, None),
            }
        }
    }
}

fn side_color(p: &Palette, side: Side) -> Color32 {
    match side {
        Side::Ours => p.accent,
        Side::Theirs => p.violet,
    }
}

/// The side's role word in accessible labels ("Use ours for conflict 1").
fn side_word(side: Side) -> &'static str {
    match side {
        Side::Ours => "ours",
        Side::Theirs => "theirs",
    }
}

fn side_pane(side: Side) -> Pane {
    match side {
        Side::Ours => Pane::Ours,
        Side::Theirs => Pane::Theirs,
    }
}

fn fill_of(state: Tri) -> Fill {
    match state {
        Tri::None => Fill::Empty,
        Tri::Some => Fill::Partial,
        Tri::All => Fill::Full,
    }
}

// ---- Header -------------------------------------------------------------------------------------------------

/// Path, conflict count, the progress strip and navigation, Save and close. Returns the edits it asks for.
fn header(ui: &mut Ui, p: &Palette, path: &Path, state: &State, saving: bool, request: &mut Option<Request>) -> Vec<Edit> {
    let mut edits = Vec::new();
    Frame::new()
        .fill(p.bg_toolbar)
        .inner_margin(Margin::symmetric(12, 7))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::GIT_MERGE).color(p.orange).size(16.0));
                let full = path.display().to_string();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| full.clone());
                let dir = path
                    .parent()
                    .map(|d| d.display().to_string())
                    .filter(|d| !d.is_empty())
                    .map(|d| format!("{d}/"));
                ui.spacing_mut().item_spacing.x = 0.0;
                if let Some(dir) = dir {
                    ui.label(RichText::new(dir).color(p.text_muted)).on_hover_text(&full);
                }
                ui.label(RichText::new(name).strong()).on_hover_text(&full);
                // The zero spacing above still applies to the gap after the name; add it explicitly.
                ui.add_space(10.0);
                ui.spacing_mut().item_spacing.x = 6.0;
                match state {
                    State::Editor(editor) => {
                        let model = &editor.model;
                        let n = model.chunk_count();
                        let text = if n == 1 {
                            "1 conflict".to_owned()
                        } else {
                            format!("{n} conflicts")
                        };
                        ui.label(RichText::new(text).color(p.text_muted));
                        if n > 0 {
                            ui.add_space(10.0);
                            progress_strip(ui, p, model, &mut edits);
                            ui.add_space(10.0);
                            navigator(ui, p, model, &mut edits);
                        }
                    }
                    State::Action(_) => {
                        ui.label(RichText::new("action needed").color(p.text_muted));
                    }
                    _ => {}
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let close = icon_button(ui, icon::X, "Close merge editor (Esc)");
                    close.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Close merge editor"));
                    if close.clicked() {
                        *request = Some(Request::Close);
                    }
                    if let State::Editor(_) = state {
                        let save = solid_button(ui, "Save", p.accent, !saving);
                        let save = if saving {
                            save.on_disabled_hover_text("Saving…")
                        } else {
                            save.on_hover_text("Write the result to the file and mark the conflict resolved")
                        };
                        if save.clicked() {
                            *request = Some(Request::Save);
                        }
                        if saving {
                            ui.add(egui::Spinner::new());
                        }
                        ui.add_space(6.0);
                        layout_button(ui, p);
                    }
                });
            });
        });
    edits
}

/// One segment per conflict, coloured by how it is resolved (split in two for both sides); the current one is
/// taller and outlined. Clicking a segment jumps to it.
fn progress_strip(ui: &mut Ui, p: &Palette, model: &MergeModel, edits: &mut Vec<Edit>) {
    const MAX_W: f32 = 260.0;
    let n = model.chunk_count();
    let gap = if n > 40 { 1.0 } else { 3.0 };
    let seg_w = ((MAX_W - gap * (n as f32 - 1.0)) / n as f32).clamp(2.0, 22.0);
    let width = seg_w * n as f32 + gap * (n as f32 - 1.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::hover());
    for (i, c) in model.chunks().iter().enumerate() {
        let x = rect.left() + i as f32 * (seg_w + gap);
        let current = i == model.current();
        let h = if current { 14.0 } else { 8.0 };
        let seg = Rect::from_min_size(pos2(x, rect.center().y - h / 2.0), Vec2::new(seg_w, h));
        let hit = Rect::from_min_size(pos2(x - gap / 2.0, rect.top()), Vec2::new(seg_w + gap, rect.height()));
        let s = status(c);
        let resp = ui
            .interact(hit, Id::new(("merge-progress", i)), Sense::click())
            .on_hover_text(format!("Conflict {}: {}", i + 1, status_text(s)))
            .on_hover_cursor(CursorIcon::PointingHand);
        resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, format!("Go to conflict {}", i + 1)));
        if resp.clicked() {
            edits.push(Edit::Jump(i));
        }
        let (a, b) = status_colors(p, c);
        let painter = ui.painter();
        match b {
            Some(b) => {
                let mid = seg.center().x;
                let left = Rect::from_min_max(seg.min, pos2(mid, seg.bottom()));
                let right = Rect::from_min_max(pos2(mid, seg.top()), seg.max);
                painter.rect_filled(
                    left,
                    CornerRadius {
                        nw: 2,
                        sw: 2,
                        ne: 0,
                        se: 0,
                    },
                    a,
                );
                painter.rect_filled(
                    right,
                    CornerRadius {
                        nw: 0,
                        sw: 0,
                        ne: 2,
                        se: 2,
                    },
                    b,
                );
            }
            None => {
                painter.rect_filled(seg, 2.0, if s == Status::Unresolved { a.gamma_multiply(0.85) } else { a });
            }
        }
        if current {
            painter.rect_stroke(seg.expand(1.5), 3.0, Stroke::new(1.0, p.text), StrokeKind::Outside);
        } else if resp.hovered() {
            painter.rect_stroke(seg.expand(1.0), 3.0, Stroke::new(1.0, p.text_muted), StrokeKind::Outside);
        }
    }
}

fn navigator(ui: &mut Ui, p: &Palette, model: &MergeModel, edits: &mut Vec<Edit>) {
    let n = model.chunk_count();
    let has_prev = model.current() > 0;
    let up = ui
        .add_enabled(has_prev, egui::Button::new(icon::CARET_LEFT).frame(false))
        .on_hover_text("Previous conflict (K)")
        .on_disabled_hover_text("This is the first conflict");
    up.widget_info(|| WidgetInfo::labeled(WidgetType::Button, has_prev, "Previous conflict"));
    if up.clicked() {
        edits.push(Edit::Prev);
    }
    ui.label(RichText::new(format!("Conflict {} of {n}", model.current() + 1)).color(p.text));
    let has_next = model.current() + 1 < n;
    let down = ui
        .add_enabled(has_next, egui::Button::new(icon::CARET_RIGHT).frame(false))
        .on_hover_text("Next conflict (J)")
        .on_disabled_hover_text("This is the last conflict");
    down.widget_info(|| WidgetInfo::labeled(WidgetType::Button, has_next, "Next conflict"));
    if down.clicked() {
        edits.push(Edit::Next);
    }
}

// ---- Action card --------------------------------------------------------------------------------------------

fn action_card(
    ui: &mut Ui,
    p: &Palette,
    path: &Path,
    conflict: &MergeConflict,
    labels: &Labels,
    acting: bool,
    request: &mut Option<Request>,
) {
    let message = labels::card_message(&path.display().to_string(), conflict, labels);
    let actions = labels::card_actions(conflict, labels);
    card(ui, p, "Action needed", |ui| {
        ui.add(egui::Label::new(&message).wrap());
        ui.add_space(4.0);
        ui.label(RichText::new("How do you want to resolve it?").color(p.text_muted));
        ui.add_space(10.0);
        ui.horizontal_wrapped(|ui| {
            for action in &actions {
                let resp = if action.danger {
                    solid_button(ui, &action.label, p.red, !acting)
                } else {
                    ui.add_enabled(!acting, egui::Button::new(&action.label))
                };
                if resp.clicked() {
                    *request = Some(Request::Resolve(action.action));
                }
            }
            if acting {
                ui.add(egui::Spinner::new());
            }
        });
    });
}

// ---- Text editor --------------------------------------------------------------------------------------------

const PANE_HEADER_H: f32 = 30.0;
const STATUS_H: f32 = 26.0;
/// Width of the result's origin ribbon (right of its line numbers).
const RIBBON_W: f32 = 4.0;
/// Space between panes (shows the border colour; splitters sit in it).
const PANE_GAP: f32 = 1.0;

fn editor_body(ui: &mut Ui, p: &Palette, path: &Path, editor: &mut Editor, labels: &Labels, saving: bool) {
    if editor.reveal {
        editor.reveal = false;
        editor.reveal_current();
    }
    let mut edits: Vec<Edit> = Vec::new();
    let full = ui.available_rect_before_wrap();
    let min_body = 2.0 * (PANE_HEADER_H + 3.0 * ROW_H);
    let body_bottom = (full.bottom() - STATUS_H).max(full.top() + min_body);
    let body = Rect::from_min_max(full.min, pos2(full.right(), body_bottom));
    let status_rect = Rect::from_min_max(pos2(full.left(), body.bottom()), pos2(full.right(), body.bottom() + STATUS_H));
    let mut layout = load_layout(ui.ctx());
    let leaves = layout.rects(body, PANE_GAP);
    // The gaps between panes show the border colour; each pane gets the editor background.
    ui.painter().rect_filled(body, 0.0, p.border);
    for (_, rect) in &leaves {
        ui.painter().rect_filled(*rect, 0.0, p.bg_graph);
    }
    let rect_of = |slot: Slot| leaves.iter().find(|(s, _)| *s == slot).map_or(Rect::NOTHING, |(_, r)| *r);
    let region = |ui: &mut Ui, rect: Rect| {
        let mut child = ui.new_child(UiBuilder::new().max_rect(rect).layout(Layout::top_down(Align::Min)));
        child.set_clip_rect(rect.intersect(ui.clip_rect()));
        child.spacing_mut().item_spacing = Vec2::ZERO;
        child
    };
    for (side, slot) in [(Side::Ours, Slot::Ours), (Side::Theirs, Slot::Theirs)] {
        let label = match side {
            Side::Ours => &labels.ours,
            Side::Theirs => &labels.theirs,
        };
        let rect = rect_of(slot);
        let mut child = region(ui, rect);
        source_pane(
            &mut child,
            p,
            editor,
            side,
            label,
            (rect.height() - PANE_HEADER_H).max(0.0),
            &mut edits,
        );
    }
    let mut child = region(ui, status_rect);
    status_bar(&mut child, p, &editor.model, &mut edits);
    for edit in edits {
        apply(editor, edit);
    }
    let rect = rect_of(Slot::Result);
    let mut child = region(ui, rect);
    result_header(&mut child, p);
    output_pane(&mut child, p, path, editor, (rect.height() - PANE_HEADER_H).max(0.0), saving);

    // Drawn after the panes so they win the pointer over the pane edges.
    let mut changed = splitters(ui, p, &mut layout, body, PANE_GAP, Id::new("merge-splitters"));
    changed |= drop_overlay(ui, p, &mut layout, &leaves, dock_drag_id("editor"), true);
    if changed {
        store_layout(ui.ctx(), &layout);
    }
    // A new arrangement changes every pane's viewport: re-reveal the current chunk once it is laid out.
    if editor.layout.as_ref().is_some_and(|old| !old.same_shape(&layout)) {
        editor.reveal = true;
        ui.ctx().request_repaint();
    }
    editor.layout = Some(layout);
    ui.advance_cursor_after_rect(Rect::from_min_max(full.min, status_rect.max));
}

// ---- Pane layout --------------------------------------------------------------------------------------------

fn layout_id() -> Id {
    Id::new("merge-pane-layout")
}

/// The saved layout (egui's persisted memory), or the default when there is none or it is damaged.
pub(super) fn load_layout(ctx: &egui::Context) -> Node {
    ctx.data_mut(|d| d.get_persisted::<Node>(layout_id()))
        .filter(Node::is_valid)
        .unwrap_or_default()
}

fn store_layout(ctx: &egui::Context, layout: &Node) {
    ctx.data_mut(|d| d.insert_persisted(layout_id(), layout.clone()));
}

/// Where the pane being dragged (if any) is kept, per drag surface (the editor, the layout menu's map).
fn dock_drag_id(surface: &str) -> Id {
    Id::new(("merge-dock-drag", surface))
}

fn dragged_slot(ctx: &egui::Context, id: Id) -> Option<Slot> {
    ctx.data(|d| d.get_temp::<Option<Slot>>(id)).flatten()
}

fn slot_color(p: &Palette, slot: Slot) -> Color32 {
    match slot {
        Slot::Ours => side_color(p, Side::Ours),
        Slot::Theirs => side_color(p, Side::Theirs),
        Slot::Result => p.text_muted,
    }
}

/// Makes `rect` a drag handle that picks `slot` up on `surface` (see [`drop_overlay`]).
fn dock_handle(ui: &Ui, rect: Rect, slot: Slot, surface: &str) -> egui::Response {
    let resp = ui
        .interact(rect, Id::new(("merge-dock-handle", surface, slot)), Sense::drag())
        .on_hover_cursor(CursorIcon::Grab);
    if resp.drag_started() {
        ui.ctx().data_mut(|d| d.insert_temp(dock_drag_id(surface), Some(slot)));
    }
    resp
}

/// Resize handles on every boundary between siblings, recursively. True if a share changed.
fn splitters(ui: &Ui, p: &Palette, node: &mut Node, rect: Rect, gap: f32, id: Id) -> bool {
    let Node::Split { axis, children, shares } = node else {
        return false;
    };
    let axis = *axis;
    let rects = dock::child_rects(axis, shares, rect, gap);
    let mut changed = false;
    for (i, (child, r)) in children.iter_mut().zip(&rects).enumerate() {
        changed |= splitters(ui, p, child, *r, gap, id.with(i));
    }
    let len = match axis {
        dock::Axis::Row => rect.width(),
        dock::Axis::Column => rect.height(),
    };
    for (i, pair) in rects.windows(2).enumerate() {
        let (a, b) = (pair[0], pair[1]);
        let (line, hit, cursor) = match axis {
            dock::Axis::Row => {
                let line = Rect::from_min_max(pos2(a.right(), rect.top()), pos2(b.left(), rect.bottom()));
                (line, line.expand2(Vec2::new(3.0, 0.0)), CursorIcon::ResizeHorizontal)
            }
            dock::Axis::Column => {
                let line = Rect::from_min_max(pos2(rect.left(), a.bottom()), pos2(rect.right(), b.top()));
                (line, line.expand2(Vec2::new(0.0, 3.0)), CursorIcon::ResizeVertical)
            }
        };
        let resp = ui.interact(hit, id.with(("splitter", i)), Sense::drag()).on_hover_cursor(cursor);
        if resp.dragged() {
            let delta = match axis {
                dock::Axis::Row => resp.drag_delta().x,
                dock::Axis::Column => resp.drag_delta().y,
            };
            if delta != 0.0 {
                dock::resize(shares, i, delta / len.max(1.0));
                changed = true;
            }
        }
        if resp.hovered() || resp.dragged() {
            let thick = match axis {
                dock::Axis::Row => line.expand2(Vec2::new(1.0, 0.0)),
                dock::Axis::Column => line.expand2(Vec2::new(0.0, 1.0)),
            };
            ui.painter().rect_filled(thick, 0.0, p.accent);
        }
    }
    changed
}

/// While a pane is dragged on `surface`: dims it, previews where it would land on the pane under the pointer
/// (an edge splits that pane, the centre swaps them) and docks it on release. True if the layout changed.
fn drop_overlay(ui: &Ui, p: &Palette, layout: &mut Node, leaves: &[(Slot, Rect)], drag: Id, labels: bool) -> bool {
    let ctx = ui.ctx();
    let Some(dragging) = dragged_slot(ctx, drag) else { return false };
    let released = !ctx.input(|i| i.pointer.primary_down());
    let pointer = ctx.pointer_latest_pos();
    let target = pointer.and_then(|pos| {
        leaves
            .iter()
            .find(|(slot, r)| *slot != dragging && r.contains(pos))
            .map(|(slot, r)| (*slot, *r, dock::zone_at(*r, pos)))
    });
    if released {
        ctx.data_mut(|d| d.remove_temp::<Option<Slot>>(drag));
        if let Some((slot, _, zone)) = target {
            layout.dock(dragging, slot, zone);
            return true;
        }
        return false;
    }
    let painter = ui.painter();
    if let Some((_, r)) = leaves.iter().find(|(s, _)| *s == dragging) {
        painter.rect_filled(*r, 0.0, p.bg_graph.gamma_multiply(0.6));
    }
    let color = slot_color(p, dragging);
    if let Some((slot, r, zone)) = target {
        let preview = dock::zone_rect(r, zone).shrink(4.0);
        painter.rect_filled(preview, 6.0, color.gamma_multiply(0.22));
        painter.rect_stroke(preview, 6.0, Stroke::new(2.0, color), StrokeKind::Inside);
        if labels {
            let text = if zone == dock::Zone::Center {
                format!("Swap {} and {}", dragging.name(), slot.name())
            } else {
                format!("Put {} here", dragging.name())
            };
            painter.text(preview.center(), Align2::CENTER_CENTER, text, FontId::proportional(14.0), p.text);
        }
    }
    if labels && let Some(pos) = pointer {
        let galley = painter.layout_no_wrap(
            format!("{} {}", icon::DOTS_SIX_VERTICAL, dragging.name()),
            FontId::proportional(12.0),
            p.text,
        );
        let chip = Rect::from_min_size(pos + Vec2::new(14.0, 10.0), galley.size() + Vec2::new(14.0, 6.0));
        painter.rect_filled(chip, 4.0, p.bg_panel);
        painter.rect_stroke(chip, 4.0, Stroke::new(1.0, color), StrokeKind::Inside);
        painter.galley(chip.min + Vec2::new(7.0, 3.0), galley, p.text);
    }
    ctx.set_cursor_icon(CursorIcon::Grabbing);
    ctx.request_repaint();
    false
}

/// Header button that opens the layout menu: an interactive map of the layout and the presets.
fn layout_button(ui: &mut Ui, p: &Palette) {
    let resp = ui
        .add(egui::Button::new(RichText::new(format!("{} Layout", icon::LAYOUT)).color(p.text_muted)).frame(false))
        .on_hover_text("Arrange the panes");
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Layout"));
    egui::Popup::from_toggle_button_response(&resp)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| layout_menu(ui, p));
}

const MAP_SIZE: Vec2 = Vec2::new(300.0, 180.0);
const THUMB_SIZE: Vec2 = Vec2::new(90.0, 56.0);

fn layout_menu(ui: &mut Ui, p: &Palette) {
    ui.set_width(MAP_SIZE.x);
    let mut layout = load_layout(ui.ctx());
    let mut changed = false;
    ui.label(RichText::new("Arrange panes").strong());
    ui.add(
        egui::Label::new(
            RichText::new(
                "Drag a pane (here or by its header in the editor) onto an edge of another to split it there, or onto its \
                 centre to swap them. Drag the gaps to resize.",
            )
            .size(11.5)
            .color(p.text_muted),
        )
        .wrap(),
    );
    ui.add_space(6.0);

    // The map: the live layout, draggable.
    let (map, _) = ui.allocate_exact_size(MAP_SIZE, Sense::hover());
    ui.painter().rect_filled(map, 4.0, p.bg_input);
    let inner = map.shrink(4.0);
    let leaves = layout.rects(inner, 4.0);
    let drag = dock_drag_id("menu");
    let dragging = dragged_slot(ui.ctx(), drag);
    for (slot, r) in &leaves {
        let resp = dock_handle(ui, *r, *slot, "menu");
        resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, format!("{} pane", slot.name())));
        let color = slot_color(p, *slot);
        let strength = if resp.hovered() && dragging.is_none() { 0.38 } else { 0.24 };
        ui.painter().rect_filled(*r, 4.0, color.gamma_multiply(strength));
        ui.painter().rect_stroke(*r, 4.0, Stroke::new(1.0, color), StrokeKind::Inside);
        ui.painter()
            .text(r.center(), Align2::CENTER_CENTER, slot.name(), FontId::proportional(13.0), p.text);
    }
    changed |= splitters(ui, p, &mut layout, inner, 4.0, Id::new("merge-map-splitters"));
    changed |= drop_overlay(ui, p, &mut layout, &leaves, drag, false);
    ui.add_space(10.0);

    ui.label(RichText::new("Presets").size(12.0).color(p.text_muted));
    ui.add_space(2.0);
    let presets = dock::presets();
    for row in presets.chunks(3) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            for (name, preset) in row {
                if preset_thumb(ui, p, name, preset, preset.same_shape(&layout)).clicked() {
                    layout = preset.clone();
                    changed = true;
                }
            }
        });
        ui.add_space(4.0);
    }
    ui.add_space(4.0);
    if ui
        .add(egui::Button::new(format!("{} Reset to default", icon::ARROW_COUNTER_CLOCKWISE)))
        .clicked()
    {
        layout = Node::default();
        changed = true;
    }
    if changed {
        store_layout(ui.ctx(), &layout);
    }
}

/// A clickable miniature of a preset with its name underneath.
fn preset_thumb(ui: &mut Ui, p: &Palette, name: &str, preset: &Node, active: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(THUMB_SIZE + Vec2::new(0.0, 16.0), Sense::click());
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let name_owned = name.to_owned();
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, active, &name_owned));
    let thumb = Rect::from_min_size(rect.min, THUMB_SIZE);
    let painter = ui.painter();
    let frame = if active {
        p.accent
    } else if resp.hovered() {
        p.text_muted
    } else {
        p.border
    };
    painter.rect_filled(thumb, 4.0, p.bg_input);
    painter.rect_stroke(thumb, 4.0, Stroke::new(if active { 2.0 } else { 1.0 }, frame), StrokeKind::Inside);
    for (slot, r) in preset.rects(thumb.shrink(5.0), 3.0) {
        let color = slot_color(p, slot);
        painter.rect_filled(r, 2.0, color.gamma_multiply(0.45));
        painter.text(
            r.center(),
            Align2::CENTER_CENTER,
            &slot.name()[..1],
            FontId::proportional(10.0),
            p.text,
        );
    }
    painter.text(
        pos2(rect.center().x, thumb.bottom() + 9.0),
        Align2::CENTER_CENTER,
        name,
        FontId::proportional(10.5),
        if active { p.text } else { p.text_muted },
    );
    resp
}

/// Header strip of a pane: a coloured underline and the title painted in `rect`'s left part.
fn pane_header_frame(ui: &mut Ui, p: &Palette, color: Color32) -> Rect {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), PANE_HEADER_H), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, p.bg_panel);
    ui.painter().line_segment(
        [pos2(rect.left(), rect.bottom() - 1.0), pos2(rect.right(), rect.bottom() - 1.0)],
        Stroke::new(2.0, color.gamma_multiply(0.7)),
    );
    rect
}

/// Paints `⠿ ● TITLE` at the left of a pane header (the grip shows it can be dragged); returns where the text
/// after it can start.
fn pane_title(ui: &Ui, p: &Palette, rect: Rect, color: Color32, title: &str) -> f32 {
    let cy = rect.center().y;
    ui.painter().text(
        pos2(rect.left() + 6.0, cy),
        Align2::LEFT_CENTER,
        icon::DOTS_SIX_VERTICAL,
        FontId::proportional(13.0),
        p.text_faint,
    );
    let x = rect.left() + 22.0;
    ui.painter().circle_filled(pos2(x + 4.0, cy), 4.5, color);
    let title = ui.painter().text(
        pos2(x + 14.0, cy),
        Align2::LEFT_CENTER,
        title,
        FontId::proportional(11.0),
        if p.dark { color } else { color.gamma_multiply(0.9) },
    );
    title.right() + 10.0
}

fn result_header(ui: &mut Ui, p: &Palette) {
    let rect = pane_header_frame(ui, p, p.text_muted);
    let handle = dock_handle(ui, rect, Slot::Result, "editor").on_hover_text("Drag to move the result pane");
    handle.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, "Result"));
    let x = pane_title(ui, p, rect, p.text, "RESULT");
    // Legend of the origin ribbon, right-aligned.
    let mut right = rect.right() - 12.0;
    for (word, color) in [
        ("unresolved", p.orange),
        ("edited", p.green),
        ("theirs", p.violet),
        ("ours", p.accent),
    ] {
        let text = ui.painter().text(
            pos2(right, rect.center().y),
            Align2::RIGHT_CENTER,
            word,
            FontId::proportional(11.0),
            p.text_muted,
        );
        let dot = Rect::from_center_size(pos2(text.left() - 7.0, rect.center().y), Vec2::new(RIBBON_W, 10.0));
        ui.painter().rect_filled(dot, 1.0, color);
        right = dot.left() - 10.0;
    }
    // The hint only when it fits left of the legend.
    let hint = ui.painter().layout_no_wrap(
        "editable · this is what gets saved".to_owned(),
        FontId::proportional(12.0),
        p.text_faint,
    );
    if x + hint.size().x + 12.0 < right {
        ui.painter()
            .galley(pos2(x, rect.center().y - hint.size().y / 2.0), hint, p.text_faint);
    }
}

fn source_header(ui: &mut Ui, p: &Palette, model: &MergeModel, side: Side, label: &labels::SideLabel, edits: &mut Vec<Edit>) {
    let color = side_color(p, side);
    let rect = pane_header_frame(ui, p, color);
    let title = match side {
        Side::Ours => "OURS",
        Side::Theirs => "THEIRS",
    };
    let x = pane_title(ui, p, rect, color, title);
    let mut right = rect.right() - 10.0;
    if model.chunk_count() > 0 {
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(Rect::from_min_max(pos2(x, rect.top()), pos2(right, rect.bottom())))
                .layout(Layout::right_to_left(Align::Center)),
        );
        let all = model.side_state(side);
        let glyph = match all {
            Tri::All => icon::CHECK_CIRCLE,
            Tri::Some => icon::CIRCLE_HALF,
            Tri::None => icon::CIRCLE,
        };
        let resp = rows::pill(
            &mut child,
            p,
            color,
            glyph,
            "Use all",
            all == Tri::All,
            &format!("Use {} for every conflict", label.name),
        );
        let a11y = format!("Use all of {}", side_word(side));
        resp.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, all == Tri::All, &a11y));
        if resp.clicked() {
            edits.push(Edit::All(side, all != Tri::All));
        }
        right = resp.rect.left() - 8.0;
    }
    // "main · 0916fe1": the name in the side's colour, the commit muted.
    let caption = label.header();
    let text_rect = Rect::from_min_max(pos2(x, rect.top()), pos2(right.max(x), rect.bottom()));
    let (word, slot) = match side {
        Side::Ours => ("Ours", Slot::Ours),
        Side::Theirs => ("Theirs", Slot::Theirs),
    };
    let handle = Rect::from_min_max(rect.min, pos2(right.max(x), rect.bottom()));
    let resp = dock_handle(ui, handle, slot, "editor").on_hover_text(format!("Drag to move the {} pane", side_word(side)));
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, format!("{word}: {caption}")));
    let mut job = LayoutJob::default();
    let font = FontId::proportional(13.0);
    match caption.split_once(" · ") {
        Some((name, rest)) => {
            job.append(name, 0.0, egui::TextFormat::simple(font.clone(), p.text));
            job.append(&format!("  {rest}"), 0.0, egui::TextFormat::simple(rows::mono(), p.text_muted));
        }
        None => job.append(&caption, 0.0, egui::TextFormat::simple(font, p.text)),
    }
    job.wrap = egui::text::TextWrapping::truncate_at_width(text_rect.width().max(0.0));
    let galley = ui.painter().layout_job(job);
    ui.painter()
        .galley(pos2(text_rect.left(), text_rect.center().y - galley.size().y / 2.0), galley, p.text);
}

fn source_pane(ui: &mut Ui, p: &Palette, editor: &mut Editor, side: Side, label: &labels::SideLabel, body_h: f32, edits: &mut Vec<Edit>) {
    source_header(ui, p, &editor.model, side, label, edits);
    let word = side_word(side);
    let pane = side_pane(side);
    let color = side_color(p, side);
    let tint = |taken: bool| color.gamma_multiply(if taken { 0.26 } else { 0.07 });
    let rule = p.text_muted;

    let model = &editor.model;
    let syntax = editor.syntax.side(side);
    let lines = model.pane_lines(pane);
    let end_chunk = (0..model.chunk_count()).any(|i| model.chunk_range(pane, i).is_some_and(|r| r.is_empty() && r.start >= lines));
    // The empty row after a final newline (and, when it runs to the end, part of the last chunk).
    let trailing_row = model.has_trailing_row(side);
    let trailing_chunk = model.trailing_row_chunk(side);
    let total_rows = lines + usize::from(end_chunk || trailing_row);
    let gutter_w = rows::gutter_width(ui, lines + usize::from(trailing_row));
    let current = model.current();
    let scroll = editor.scroll[pane_index(pane)];
    let mut area = ScrollArea::vertical()
        .id_salt(("merge-pane", word))
        .auto_shrink([false, false])
        .max_height(body_h)
        .min_scrolled_height(body_h);
    let programmatic = scroll.set.is_some();
    if let Some(y) = scroll.set {
        area = area.vertical_scroll_offset(y);
    }
    let out = area.show_rows(ui, ROW_H, total_rows, |ui, visible| {
        for r in visible {
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
            let in_chunk = model.chunk_at(pane, r);
            let starting = model.chunk_starting_at(pane, r);
            let mut style = RowStyle::default();
            // Line within its chunk, and whether the result uses it.
            let line_in_chunk = in_chunk.and_then(|i| Some((i, r - model.chunk_range(pane, i)?.start)));
            // The empty last row is used with the chunk's last line (whose newline it shows).
            let trailing = trailing_chunk.filter(|_| r == lines);
            let chunk_len = |i: usize| model.chunk_range(pane, i).map_or(0, |range| range.len());
            let mut spine_at: Option<(usize, Segment)> = None;
            if let Some((i, k)) = line_in_chunk {
                let takes = model.chunks().get(i).is_some_and(|c| c.sel.side(side).line(k));
                style.bg = Some(tint(takes));
                let last = k + 1 == chunk_len(i) && trailing != Some(i);
                if i == current {
                    style.rule_top = (k == 0).then_some(rule);
                    style.rule_bottom = last.then_some(rule);
                }
                spine_at = Some((i, Segment { first: k == 0, last }));
            }
            if let Some(i) = trailing {
                let takes = model.chunks().get(i).is_some_and(|c| {
                    let chosen = c.sel.side(side);
                    match chunk_len(i) {
                        0 => chosen.is_taken(),
                        n => chosen.line(n - 1),
                    }
                });
                style.bg = Some(tint(takes));
                if i == current {
                    style.rule_bottom = Some(rule);
                }
                if chunk_len(i) > 0 {
                    spine_at = Some((i, Segment { first: false, last: true }));
                }
            }
            let empty_start = starting.filter(|&i| model.chunk_range(pane, i).is_some_and(|c| c.is_empty()));
            if let Some(i) = empty_start {
                style.notch = Some(if i == current { rule } else { color });
            }
            let text = model.side_line(side, r).unwrap_or("");
            let colors = syntax.line_colors(model.side_text(side), r, text.trim_end_matches(['\n', '\r']), p);
            let plain_number = (line_in_chunk.is_none() && (r < lines || trailing_row)).then_some(r + 1);
            rows::paint_row(ui, rect, p, SPINE_COL, gutter_w, plain_number, text, &colors, style);
            // Clicking a chunk line anywhere uses or drops it (as its number chip does), and makes its chunk current.
            let resp = if line_in_chunk.is_some() {
                resp.on_hover_cursor(CursorIcon::PointingHand)
            } else {
                resp
            };
            if resp.clicked()
                && let Some(i) = in_chunk.or(starting).or(trailing)
            {
                edits.push(Edit::Current(i));
                if let Some((i, k)) = line_in_chunk {
                    edits.push(Edit::ToggleLine(i, side, k));
                }
            }
            let spine_rect = Rect::from_min_size(rect.min, Vec2::new(SPINE_COL, ROW_H));
            // Whole-chunk choice: the spine bar alongside the chunk.
            if let Some((i, segment)) = spine_at {
                let state = model.chunks().get(i).map_or(Tri::None, |c| c.sel.side(side).state());
                let a11y = format!("Use {word} for conflict {}", i + 1);
                let tooltip = if state == Tri::All {
                    format!("Stop using {} for conflict {}", label.name, i + 1)
                } else {
                    format!("Use all of {} for conflict {} (O)", label.name, i + 1)
                };
                let resp = rows::spine(
                    ui,
                    spine_rect,
                    Id::new(("merge-spine", word, r)),
                    color,
                    fill_of(state),
                    segment,
                    i == current,
                    segment.first.then_some(a11y.as_str()),
                    &tooltip,
                );
                if resp.clicked() {
                    edits.push(Edit::Toggle(i, side));
                }
            }
            if let Some(i) = empty_start {
                let taken = model.chunks().get(i).is_some_and(|c| c.sel.side(side).is_taken());
                let resp = rows::spine_dot(
                    ui,
                    spine_rect,
                    Id::new(("merge-spine-dot", word, i)),
                    color,
                    taken,
                    &format!("Use {word} for conflict {}", i + 1),
                    &format!(
                        "{} removed these lines: use its version (nothing) for conflict {}",
                        label.name,
                        i + 1
                    ),
                );
                if resp.clicked() {
                    edits.push(Edit::Toggle(i, side));
                }
            }
            // Per-line choice: the line number is a chip that puts just this line in the result.
            if let Some((i, k)) = line_in_chunk {
                let taken = model.chunks().get(i).is_some_and(|c| c.sel.side(side).line(k));
                let pick_rect = Rect::from_min_size(rect.min + Vec2::new(SPINE_COL, 0.0), Vec2::new(gutter_w, ROW_H));
                let resp = rows::line_pick(
                    ui,
                    pick_rect,
                    Id::new(("merge-line", word, i, k)),
                    p,
                    color,
                    r + 1,
                    taken,
                    ui.rect_contains_pointer(rect),
                    &format!("Use {word} line {} of conflict {}", k + 1, i + 1),
                    if taken {
                        "Remove this line from the result"
                    } else {
                        "Add this line to the result"
                    },
                );
                if resp.clicked() {
                    edits.push(Edit::Current(i));
                    edits.push(Edit::ToggleLine(i, side, k));
                }
            }
        }
    });
    record_scroll(ui, editor, pane, &out, ROW_H, programmatic);
}

fn record_scroll<R>(
    ui: &Ui,
    editor: &mut Editor,
    pane: Pane,
    out: &egui::scroll_area::ScrollAreaOutput<R>,
    row_h: f32,
    programmatic: bool,
) {
    let offset = out.state.offset.y;
    let s = editor.scroll_mut(pane);
    let user_scrolled = !programmatic && (offset - s.last).abs() > 0.5;
    s.set = None;
    s.last = offset;
    s.viewport_h = out.inner_rect.height();
    s.content_h = out.content_size.y;
    s.row_h = row_h;
    if user_scrolled {
        editor.sync_from(pane, offset);
        // Panes drawn earlier this frame only apply the synced offset next frame; without a repaint they would
        // lag behind (until the next input event) once the wheel's smooth scroll settles.
        ui.ctx().request_repaint();
    }
}

/// Bottom bar: undo / redo / reset, a hint, and how many conflicts are resolved.
fn status_bar(ui: &mut Ui, p: &Palette, model: &MergeModel, edits: &mut Vec<Edit>) {
    let rect = ui.max_rect();
    ui.painter().rect_filled(rect, 0.0, p.bg_toolbar);
    ui.painter()
        .line_segment([rect.left_top(), rect.right_top()], Stroke::new(1.0, p.border));
    let mut bar = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(8.0, 0.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    let ui = &mut bar;
    ui.spacing_mut().item_spacing = Vec2::new(2.0, 0.0);
    let small = |glyph: &str, text: &str| egui::Button::new(RichText::new(format!("{glyph} {text}")).size(12.0)).frame(false);
    if ui
        .add_enabled(model.can_undo(), small(icon::ARROW_U_UP_LEFT, "Undo"))
        .on_hover_text("Undo (⌘Z)")
        .on_disabled_hover_text("Nothing to undo")
        .clicked()
    {
        edits.push(Edit::Undo);
    }
    if ui
        .add_enabled(model.can_redo(), small(icon::ARROW_U_UP_RIGHT, "Redo"))
        .on_hover_text("Redo (⌘⇧Z)")
        .on_disabled_hover_text("Nothing to redo")
        .clicked()
    {
        edits.push(Edit::Redo);
    }
    let n = model.chunk_count();
    let dirty = model.is_dirty() || model.undecided() < n;
    if ui
        .add_enabled(dirty, small(icon::ERASER, "Reset"))
        .on_hover_text("Restore the result to the auto-merged template")
        .on_disabled_hover_text("Nothing to reset")
        .clicked()
    {
        edits.push(Edit::Reset);
    }
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing = Vec2::new(10.0, 0.0);
        let undecided = model.undecided();
        let status = if n == 0 {
            RichText::new("No conflicting changes: review the result and Save.").color(p.text_muted)
        } else if undecided == 0 {
            RichText::new(format!("{} All {n} resolved", icon::CHECK)).color(p.green)
        } else {
            RichText::new(format!("{} of {n} resolved", n - undecided)).color(p.orange)
        };
        ui.label(status.size(12.0));
        if n > 0 {
            ui.add(
                egui::Label::new(
                    RichText::new("Bar: whole conflict  ·  Line number: single line  ·  O / T: ours / theirs  ·  J / K: move")
                        .size(11.0)
                        .color(p.text_faint),
                )
                .truncate(),
            );
        }
    });
}

/// The editable result; read-only while a save is running (edits made then would be lost).
fn output_pane(ui: &mut Ui, p: &Palette, path: &Path, editor: &mut Editor, height: f32, saving: bool) {
    let font = rows::mono();
    let row_h = ui.ctx().fonts_mut(|f| f.row_height(&font));
    let lines = editor.model.pane_lines(Pane::Output);
    let gutter_w = rows::gutter_width(ui, lines);
    let text_left_pad = gutter_w + RIBBON_W + 8.0;
    let scroll = editor.scroll[pane_index(Pane::Output)];
    let programmatic = scroll.set.is_some();
    let mut area = ScrollArea::vertical()
        .id_salt("merge-output-scroll")
        .auto_shrink([false, false])
        .max_height(height)
        .min_scrolled_height(height);
    if let Some(y) = scroll.set {
        area = area.vertical_scroll_offset(y);
    }
    let text_color = p.text;
    let now = ui.input(|i| i.time);
    let out = area.show(ui, |ui| {
        let bg = ui.painter().add(Shape::Noop);
        let output_syntax = &mut editor.output_syntax;
        let syntax_path = editor.syntax.path.as_path();
        let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
            let text = buf.as_str();
            let colors = output_syntax.highlights(syntax_path, text).colors(0..text.len(), p);
            ui.painter().layout_job(output_job(text, &colors, font.clone(), text_color))
        };
        let margin = Margin {
            left: text_left_pad.min(120.0) as i8,
            right: 4,
            top: 0,
            bottom: 0,
        };
        let output = egui::TextEdit::multiline(editor.model.text_mut())
            .id(output_id(path))
            .code_editor()
            .frame(Frame::NONE.inner_margin(margin))
            .desired_width(f32::INFINITY)
            .desired_rows(1)
            .interactive(!saving)
            .layouter(&mut layouter)
            .show(ui);
        output
            .response
            .response
            .widget_info(|| WidgetInfo::labeled(WidgetType::TextEdit, true, "Merge output"));
        if output.response.response.changed() {
            editor.model.sync_after_edit(now);
        }
        // Backgrounds, the origin ribbon and line numbers, for visible rows only; state chips on top of the text.
        let model = &editor.model;
        let rect = output.response.response.rect;
        let clip = ui.clip_rect();
        let galley = &output.galley;
        let origin = output.galley_pos;
        let first = galley.rows.partition_point(|r| origin.y + r.rect().max.y < clip.top());
        let ribbon_x = rect.left() + gutter_w;
        let band_left = ribbon_x + RIBBON_W;
        let current = model.current();
        let rule = p.text_muted;
        let mut shapes = vec![Shape::rect_filled(
            Rect::from_min_max(pos2(rect.left(), clip.top()), pos2(ribbon_x, clip.bottom())),
            0.0,
            p.bg_graph_alt,
        )];
        let mut chips: Vec<(usize, f32)> = Vec::new();
        let painter = ui.painter();
        for (j, row) in galley.rows.iter().enumerate().skip(first) {
            let row_rect = row.rect().translate(origin.to_vec2());
            if row_rect.top() > clip.bottom() {
                break;
            }
            let top = row_rect.top();
            let band = Rect::from_min_max(pos2(band_left, top), pos2(rect.right(), top + row_h));
            if let Some(i) = model.chunk_at(Pane::Output, j) {
                let c = &model.chunks()[i];
                let k = j - c.range.start;
                let s = status(c);
                let line_color = match s {
                    Status::Unresolved => p.orange,
                    Status::Edited => p.green,
                    _ if k < c.kept_lines() => p.green,
                    _ => model.output_line_side(i, k).map_or(p.text_muted, |side| side_color(p, side)),
                };
                let strength = if s == Status::Unresolved { 0.13 } else { 0.09 };
                shapes.push(Shape::rect_filled(band, 0.0, line_color.gamma_multiply(strength)));
                let ribbon = Rect::from_min_size(pos2(ribbon_x, top), Vec2::new(RIBBON_W, row_h));
                if s == Status::Unresolved {
                    // Striped: the result still holds the base lines here.
                    let half = Rect::from_min_size(ribbon.min, Vec2::new(RIBBON_W, row_h * 0.6));
                    shapes.push(Shape::rect_filled(half, 0.0, line_color));
                } else {
                    shapes.push(Shape::rect_filled(ribbon, 0.0, line_color));
                }
                if i == current {
                    if k == 0 {
                        shapes.push(Shape::line_segment(
                            [pos2(band_left, top + 0.5), pos2(rect.right(), top + 0.5)],
                            Stroke::new(1.0, rule),
                        ));
                    }
                    if k + 1 == c.range.len() {
                        let y = top + row_h - 0.5;
                        shapes.push(Shape::line_segment(
                            [pos2(band_left, y), pos2(rect.right(), y)],
                            Stroke::new(1.0, rule),
                        ));
                    }
                }
                if k == 0 {
                    chips.push((i, top));
                }
            }
            for (i, c) in model.chunks().iter().enumerate() {
                if c.range.is_empty() && c.range.start == j {
                    let color = if i == current { rule } else { status_colors(p, c).0 };
                    rows::dashed(painter, band_left, rect.right(), top + 1.0, color);
                    chips.push((i, top));
                }
            }
            if j < lines {
                painter.text(
                    pos2(ribbon_x - 8.0, band.center().y),
                    Align2::RIGHT_CENTER,
                    (j + 1).to_string(),
                    font.clone(),
                    p.text_faint,
                );
            }
        }
        painter.set(bg, Shape::Vec(shapes));
        for (i, top) in chips {
            state_chip(ui, p, &model.chunks()[i], pos2(clip.right() - 10.0, top + row_h / 2.0));
        }
    });
    record_scroll(ui, editor, Pane::Output, &out, row_h, programmatic);
}

/// The small status label ("OURS", "UNRESOLVED", …) at the right end of a chunk's first result row.
fn state_chip(ui: &Ui, p: &Palette, c: &ChunkState, right_center: Pos2) {
    let s = status(c);
    let (a, b) = status_colors(p, c);
    let text = status_text(s).to_uppercase();
    let galley = ui
        .painter()
        .layout_no_wrap(text, FontId::proportional(9.5), if b.is_some() { p.text } else { a });
    let size = galley.size() + Vec2::new(10.0, 2.0);
    let chip = Rect::from_min_size(pos2(right_center.x - size.x, right_center.y - size.y / 2.0), size);
    let painter = ui.painter();
    painter.rect_filled(chip, 3.0, p.bg_panel);
    painter.rect_stroke(chip, 3.0, Stroke::new(1.0, a), StrokeKind::Inside);
    if let Some(b) = b {
        // Both sides: the right half of the outline in theirs' colour.
        let right = Rect::from_min_max(pos2(chip.center().x, chip.top()), chip.max);
        painter
            .with_clip_rect(right)
            .rect_stroke(chip, 3.0, Stroke::new(1.0, b), StrokeKind::Inside);
    }
    painter.galley(chip.center() - galley.size() / 2.0, galley, a);
}

/// Layout job for the whole output text: `colors` (sorted, non-overlapping byte ranges) over `color`.
fn output_job(text: &str, colors: &[(std::ops::Range<usize>, Color32)], font: egui::FontId, color: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let mut append = |range: std::ops::Range<usize>, c: Color32| {
        if let Some(s) = text.get(range).filter(|s| !s.is_empty()) {
            job.append(s, 0.0, egui::TextFormat::simple(font.clone(), c));
        }
    };
    let mut pos = 0;
    for (r, c) in colors {
        if r.start < pos || !text.is_char_boundary(r.start) || !text.is_char_boundary(r.end.min(text.len())) {
            continue;
        }
        append(pos..r.start, color);
        append(r.start..r.end.min(text.len()), *c);
        pos = r.end.min(text.len());
    }
    append(pos..text.len(), color);
    job
}

#[cfg(test)]
mod scroll_sync_tests {
    use super::{anchor_offset, anchor_position, joint_anchors};

    /// Maps `offset` of pane `from` to pane `to` through the joint anchors.
    fn map(anchors: &[Vec<f32>; 3], from: usize, to: usize, offset: f32) -> f32 {
        let (seg, p) = anchor_position(&anchors[from], offset);
        anchor_offset(&anchors[to], seg, p)
    }

    #[test]
    fn edge_chunks_clamped_in_one_pane_do_not_cause_jumps() {
        // Chunk 0 sits above the top of pane 0 (would clamp to 0) but well inside panes 1 and 2; chunk 2 sits past
        // pane 1's bottom. Both are dropped, so no pane gets a zero-length segment another pane doesn't.
        let chunks = [
            [Some(-50.0), Some(200.0), Some(150.0)],
            [Some(400.0), Some(500.0), Some(450.0)],
            [Some(900.0), Some(1200.0), Some(950.0)],
            [None, Some(10.0), Some(10.0)],
        ];
        let anchors = joint_anchors(&chunks, [1000.0, 1000.0, 1100.0]);
        assert_eq!(anchors[0], vec![0.0, 400.0, 1000.0]);
        assert_eq!(anchors[1], vec![0.0, 500.0, 1000.0]);
        assert_eq!(anchors[2], vec![0.0, 450.0, 1100.0]);
        // Scrolling any pane moves the others continuously (no step bigger than the slope allows) and the
        // mapping round-trips, so switching which pane is scrolled never makes the others jump.
        for from in 0..3 {
            for to in 0..3 {
                let mut prev = map(&anchors, from, to, 0.0);
                for y in 1..=1000 {
                    let mapped = map(&anchors, from, to, y as f32);
                    assert!(mapped >= prev && mapped - prev < 3.0, "{from}->{to} jumped at {y}");
                    let back = map(&anchors, to, from, mapped);
                    assert!((back - y as f32).abs() < 1e-2, "{from}->{to}->{from} at {y} gave {back}");
                    prev = mapped;
                }
            }
        }
    }

    #[test]
    fn non_scrollable_pane_stays_at_top() {
        let chunks = [[Some(300.0), Some(10.0), Some(400.0)]];
        let anchors = joint_anchors(&chunks, [800.0, 0.0, 900.0]);
        assert_eq!(anchors[0], vec![0.0, 300.0, 800.0]);
        assert_eq!(anchors[1], vec![0.0, 0.0, 0.0]);
        assert_eq!(map(&anchors, 0, 1, 500.0), 0.0);
        assert!((map(&anchors, 0, 2, 300.0) - 400.0).abs() < 1e-4);
    }

    #[test]
    fn maps_proportionally_between_anchors() {
        // Pane A: chunk anchors at 100 and 1100 (a long chunk between); pane B: at 100 and 200.
        let a = [0.0, 100.0, 1100.0, 1500.0];
        let b = [0.0, 100.0, 200.0, 400.0];
        let (seg, p) = anchor_position(&a, 600.0);
        assert_eq!(seg, 1);
        assert!((p - 0.5).abs() < 1e-6);
        assert!((anchor_offset(&b, seg, p) - 150.0).abs() < 1e-4);
        // Every step in A moves B (no stall-then-jump through the long chunk).
        let mut prev = -1.0;
        for y in (0..=1500).step_by(10) {
            let (seg, p) = anchor_position(&a, y as f32);
            let mapped = anchor_offset(&b, seg, p);
            assert!(mapped > prev, "B stalled at A={y}");
            prev = mapped;
        }
    }

    #[test]
    fn anchors_line_up_and_ends_map_to_ends() {
        let a = [0.0, 0.0, 300.0, 900.0];
        let b = [0.0, 40.0, 700.0, 700.0];
        assert_eq!(anchor_offset(&b, anchor_position(&a, 0.0).0, anchor_position(&a, 0.0).1), 0.0);
        let (seg, p) = anchor_position(&a, 300.0);
        assert_eq!(anchor_offset(&b, seg, p), 700.0);
        let (seg, p) = anchor_position(&a, 900.0);
        assert_eq!(anchor_offset(&b, seg, p), 700.0);
        // Past the end clamps.
        let (seg, p) = anchor_position(&a, 5000.0);
        assert_eq!(anchor_offset(&b, seg, p), 700.0);
    }
}
