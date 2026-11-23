//! The merge editor's source-pane rows: a clickable *spine* column (one bar per conflict, to use the whole chunk),
//! a line-number column whose numbers double as per-line pick chips, tinted backgrounds and rules framing the
//! current chunk. Row height, font and text layout are the diff viewer's ([`crate::ui::diff::rows`]); only the
//! merge-specific decoration lives here.

use std::ops::Range;

use egui::text::LayoutJob;
use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2, WidgetInfo, WidgetType};

use crate::theme::Palette;
use crate::ui::diff::rows::{FONT_SIZE, ROW_HEIGHT, text_job};

/// Row height of the source panes (same as the diff viewer's).
pub const ROW_H: f32 = ROW_HEIGHT;
/// Width of the spine column left of the line numbers.
pub const SPINE_COL: f32 = 18.0;

/// How much of a side a chunk (or the whole file) uses: none, some lines, every line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    Empty,
    Partial,
    Full,
}

const TAB: &str = "    ";
const TEXT_PAD: f32 = 8.0;

pub fn mono() -> FontId {
    FontId::monospace(FONT_SIZE)
}

/// Width of a line-number column for `lines` lines (room for the pick chip around the number).
pub fn gutter_width(ui: &Ui, lines: usize) -> f32 {
    let digits = lines.max(1).to_string().len().max(3);
    let w = ui.ctx().fonts_mut(|f| f.glyph_width(&mono(), '0'));
    w * digits as f32 + 16.0
}

/// How one row looks.
#[derive(Debug, Clone, Copy, Default)]
pub struct RowStyle {
    pub bg: Option<Color32>,
    /// Rule along the top / bottom edge (frames the current chunk).
    pub rule_top: Option<Color32>,
    pub rule_bottom: Option<Color32>,
    /// Dashed line along the top edge: a chunk with no lines on this side sits here.
    pub notch: Option<Color32>,
}

/// Layout job for one line: `colors` (byte ranges of the line without its terminator, with their text colour)
/// over `color`, tabs expanded (the ranges are remapped). Ranges that overlap an earlier one or fall off char
/// boundaries are skipped.
pub fn line_job(line: &str, colors: &[(Range<usize>, Color32)], color: Color32) -> LayoutJob {
    let line = line.trim_end_matches(['\n', '\r']);
    let mut kept: Vec<(Range<usize>, Color32)> = Vec::with_capacity(colors.len());
    for (r, c) in colors {
        let r = r.start..r.end.min(line.len());
        let after_previous = kept.last().is_none_or(|(prev, _)| prev.end <= r.start);
        if after_previous && line.get(r.clone()).is_some() {
            kept.push((r, *c));
        }
    }
    let (text, kept) = expand_tabs(line, kept);
    text_job(&text, &mono(), color, &kept, &[], Color32::TRANSPARENT, false)
}

/// `line` with each tab replaced by [`TAB`], and `ranges` (sorted, on char boundaries) moved to match.
fn expand_tabs(line: &str, mut ranges: Vec<(Range<usize>, Color32)>) -> (String, Vec<(Range<usize>, Color32)>) {
    if !line.contains('\t') {
        return (line.to_owned(), ranges);
    }
    let shift = |at: usize| line.as_bytes()[..at].iter().filter(|&&b| b == b'\t').count() * (TAB.len() - 1);
    for (r, _) in &mut ranges {
        *r = r.start + shift(r.start)..r.end + shift(r.end);
    }
    (line.replace('\t', TAB), ranges)
}

/// Dashed horizontal line from `x0` to `x1` at `y`.
pub fn dashed(painter: &egui::Painter, x0: f32, x1: f32, y: f32, color: Color32) {
    let (dash, gap) = (6.0, 4.0);
    let mut x = x0;
    while x < x1 {
        painter.line_segment([Pos2::new(x, y), Pos2::new((x + dash).min(x1), y)], Stroke::new(2.0, color));
        x += dash + gap;
    }
}

/// Paints one row: `[spine (spine_w)][line numbers (gutter_w)][text]` inside `rect` (text clipped to the row), the
/// text in its syntax `colors` (see [`line_job`]). `number` is drawn plainly; chunk lines leave it `None` and draw
/// a [`line_pick`] instead. The background and rules start after the spine.
#[allow(clippy::too_many_arguments)]
pub fn paint_row(
    ui: &Ui,
    rect: Rect,
    p: &Palette,
    spine_w: f32,
    gutter_w: f32,
    number: Option<usize>,
    text: &str,
    colors: &[(Range<usize>, Color32)],
    style: RowStyle,
) {
    let painter = ui.painter_at(rect);
    let band_left = rect.left() + spine_w;
    if let Some(bg) = style.bg {
        painter.rect_filled(Rect::from_min_max(Pos2::new(band_left, rect.top()), rect.max), 0.0, bg);
    }
    if let Some(c) = style.rule_top {
        painter.line_segment(
            [Pos2::new(band_left, rect.top() + 0.5), Pos2::new(rect.right(), rect.top() + 0.5)],
            Stroke::new(1.0, c),
        );
    }
    if let Some(c) = style.rule_bottom {
        painter.line_segment(
            [
                Pos2::new(band_left, rect.bottom() - 0.5),
                Pos2::new(rect.right(), rect.bottom() - 0.5),
            ],
            Stroke::new(1.0, c),
        );
    }
    if let Some(c) = style.notch {
        dashed(&painter, band_left + gutter_w, rect.right(), rect.top() + 1.0, c);
    }
    if let Some(n) = number {
        painter.text(
            Pos2::new(band_left + gutter_w - 8.0, rect.center().y),
            Align2::RIGHT_CENTER,
            n.to_string(),
            mono(),
            p.text_faint,
        );
    }
    let galley = painter.layout_job(line_job(text, colors, p.text));
    let pos = Pos2::new(band_left + gutter_w + TEXT_PAD, rect.center().y - galley.size().y / 2.0);
    painter.galley(pos, galley, p.text);
}

/// Where a row sits in its chunk's spine (the bar is rounded at its ends).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub first: bool,
    pub last: bool,
}

/// One row's piece of a chunk's spine bar in the spine column `rect`: faint while the chunk uses none of the side,
/// half-strength for some lines, solid for all. Wider for the current chunk. Only the chunk's first row carries
/// the accessible `label` (so each chunk has one named control); every row is clickable.
#[allow(clippy::too_many_arguments)]
pub fn spine(
    ui: &mut Ui,
    rect: Rect,
    id: egui::Id,
    color: Color32,
    fill: Fill,
    segment: Segment,
    current: bool,
    label: Option<&str>,
    tooltip: &str,
) -> Response {
    let resp = ui
        .interact(rect, id, Sense::click())
        .on_hover_text(tooltip)
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if let Some(label) = label {
        let label = label.to_owned();
        resp.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, fill == Fill::Full, &label));
    }
    let w = if current { 8.0 } else { 5.0 };
    let top = rect.top() + if segment.first { 2.0 } else { 0.0 };
    let bottom = rect.bottom() - if segment.last { 2.0 } else { 0.0 };
    let bar = Rect::from_x_y_ranges(rect.center().x - w / 2.0..=rect.center().x + w / 2.0, top..=bottom);
    let r = (w / 2.0) as u8;
    let radius = CornerRadius {
        nw: if segment.first { r } else { 0 },
        ne: if segment.first { r } else { 0 },
        sw: if segment.last { r } else { 0 },
        se: if segment.last { r } else { 0 },
    };
    let strength = match fill {
        Fill::Empty => 0.28,
        Fill::Partial => 0.6,
        Fill::Full => 1.0,
    };
    let strength = if resp.hovered() && fill != Fill::Full {
        strength + 0.25
    } else {
        strength
    };
    ui.painter().rect_filled(bar, radius, color.gamma_multiply(strength));
    resp
}

/// The spine marker of a chunk with no lines on this side (it deleted them): a ring that fills once the side is
/// used, at the top of the row the chunk sits before.
pub fn spine_dot(ui: &mut Ui, rect: Rect, id: egui::Id, color: Color32, taken: bool, label: &str, tooltip: &str) -> Response {
    let resp = ui
        .interact(rect, id, Sense::click())
        .on_hover_text(tooltip)
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let label = label.to_owned();
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, taken, &label));
    let c = Pos2::new(rect.center().x, rect.top() + 5.0);
    if taken || resp.hovered() {
        ui.painter()
            .circle_filled(c, 4.0, if taken { color } else { color.gamma_multiply(0.5) });
    } else {
        ui.painter().circle_stroke(c, 3.5, Stroke::new(1.5, color));
    }
    resp
}

/// The line number of a chunk line as a pick chip in `rect` (the line-number column): a plain number while the line
/// isn't used, outlined in `color` while the row is hovered, a solid chip once it is in the result. `row_hovered`
/// is whether the pointer is anywhere on the row; only clicks on `rect` itself count.
#[allow(clippy::too_many_arguments)]
pub fn line_pick(
    ui: &mut Ui,
    rect: Rect,
    id: egui::Id,
    p: &Palette,
    color: Color32,
    number: usize,
    taken: bool,
    row_hovered: bool,
    label: &str,
    tooltip: &str,
) -> Response {
    let resp = ui
        .interact(rect, id, Sense::click())
        .on_hover_text(tooltip)
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let label = label.to_owned();
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, taken, &label));
    let chip = Rect::from_min_max(
        Pos2::new(rect.left() + 3.0, rect.top() + 2.0),
        Pos2::new(rect.right() - 3.0, rect.bottom() - 2.0),
    );
    let hovered = row_hovered || resp.hovered();
    let painter = ui.painter();
    let text_color = if taken {
        painter.rect_filled(chip, 3.0, color);
        if p.dark { Color32::from_gray(16) } else { Color32::WHITE }
    } else if hovered {
        painter.rect_stroke(chip, 3.0, Stroke::new(1.0, color), StrokeKind::Inside);
        color
    } else {
        p.text_faint
    };
    painter.text(
        Pos2::new(rect.right() - 8.0, rect.center().y),
        Align2::RIGHT_CENTER,
        number.to_string(),
        mono(),
        text_color,
    );
    resp
}

/// A small pill button (pane headers): `glyph label`, filled in `color` when `on`.
pub fn pill(ui: &mut Ui, p: &Palette, color: Color32, glyph: &str, text: &str, on: bool, tooltip: &str) -> Response {
    let font = FontId::proportional(12.0);
    let galley = ui.painter().layout_no_wrap(format!("{glyph} {text}"), font, Color32::PLACEHOLDER);
    let size = Vec2::new(galley.size().x + 16.0, 20.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let resp = resp.on_hover_text(tooltip);
    let text_color = if on {
        ui.painter()
            .rect_filled(rect, 10.0, color.gamma_multiply(if resp.hovered() { 0.85 } else { 0.7 }));
        if p.dark { Color32::from_gray(16) } else { Color32::WHITE }
    } else {
        let stroke = if resp.hovered() { color } else { color.gamma_multiply(0.6) };
        ui.painter().rect_stroke(rect, 10.0, Stroke::new(1.0, stroke), StrokeKind::Inside);
        color
    };
    let pos = rect.center() - galley.size() / 2.0;
    ui.painter().galley(pos, galley, text_color);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_job_strips_terminators_and_expands_tabs() {
        let text = |line: &str| line_job(line, &[], Color32::WHITE).text;
        assert_eq!(text("a\tb\r\n"), "a    b");
        assert_eq!(text("x"), "x");
        assert_eq!(text("\n"), "");
    }

    #[test]
    fn line_job_moves_ranges_past_tabs() {
        let job = line_job("a\tb\tc", &[(2..3, Color32::RED), (4..5, Color32::BLUE)], Color32::WHITE);
        assert_eq!(job.text, "a    b    c");
        let colored: Vec<(&str, Color32)> = job
            .sections
            .iter()
            .map(|s| (&job.text[s.byte_range.start.0..s.byte_range.end.0], s.format.color))
            .filter(|(_, c)| *c != Color32::WHITE)
            .collect();
        assert_eq!(colored, [("b", Color32::RED), ("c", Color32::BLUE)]);
    }

    #[test]
    fn line_job_colours_ranges_and_expands_tabs() {
        let job = line_job(
            "\tlet x;\n",
            &[(1..4, Color32::RED), (2..3, Color32::BLUE), (8..20, Color32::RED)],
            Color32::WHITE,
        );
        assert_eq!(job.text, "    let x;");
        let parts: Vec<(&str, Color32)> = job
            .sections
            .iter()
            .map(|s| (&job.text[s.byte_range.start.0..s.byte_range.end.0], s.format.color))
            .collect();
        // The overlapping and out-of-range ranges are skipped.
        assert_eq!(parts, [("    ", Color32::WHITE), ("let", Color32::RED), (" x;", Color32::WHITE)]);
    }
}
