//! Code-row primitives: fixed-height monospace rows virtualised with `ScrollArea::show_rows`, line-number gutters,
//! `+`/`−` signs, add/del backgrounds, intra-line highlight sections and a shared horizontal text offset. The merge
//! editor's panes (`ui::merge::rows`) use the row height, font and [`text_job`] from here.
//!
//! Horizontal scrolling (decision, plan WP13): the gutters stay put and only the text moves. The view keeps one
//! horizontal offset per diff and every text column paints with it, so in Split mode both columns scroll
//! horizontally **together**, just like they scroll vertically together (one vertical `ScrollArea` drives both
//! columns). The offset changes with horizontal wheel / trackpad input (or Shift+wheel) over the rows and with
//! the bar drawn by [`h_scrollbar`].

use std::ops::Range;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, Painter, Pos2, Rect, Sense, Ui, Vec2};

/// Row height (reference UI §0: diff rows 20 px, fixed for virtualisation).
pub(crate) const ROW_HEIGHT: f32 = 20.0;
/// Monospace size (§0).
pub(crate) const FONT_SIZE: f32 = 12.0;
const GUTTER_PAD: f32 = 8.0;
const SIGN_WIDTH: f32 = 16.0;

/// Font metrics for one frame.
#[derive(Debug, Clone)]
pub(crate) struct Metrics {
    pub font: FontId,
    pub char_width: f32,
    /// Width of one line-number column (fits `digits` digits).
    pub number_width: f32,
}

impl Metrics {
    pub(crate) fn new(ui: &Ui, max_lineno: u32) -> Self {
        let font = FontId::monospace(FONT_SIZE);
        let char_width = ui.ctx().fonts_mut(|f| f.glyph_width(&font, '0')).max(1.0);
        let digits = max_lineno.max(1).ilog10() as f32 + 1.0;
        Self {
            font,
            char_width,
            number_width: digits.max(3.0) * char_width + GUTTER_PAD,
        }
    }

    /// Gutter width for `columns` number columns plus the sign column.
    pub(crate) fn gutter_width(&self, columns: usize) -> f32 {
        self.number_width * columns as f32 + SIGN_WIDTH
    }

    /// Width of `chars` characters of text (horizontal extent).
    pub(crate) fn text_width(&self, chars: usize) -> f32 {
        chars as f32 * self.char_width + 2.0 * GUTTER_PAD
    }
}

/// What one painted code line looks like.
pub(crate) struct LineSpec<'a> {
    /// Row background (`TRANSPARENT` for none).
    pub bg: Color32,
    /// Line numbers, one per gutter column (`None` = blank).
    pub numbers: &'a [Option<u32>],
    pub number_color: Color32,
    pub sign: &'a str,
    pub sign_color: Color32,
    pub text: &'a str,
    pub text_color: Color32,
    /// Syntax colours: byte ranges of `text` and their text colour.
    pub syntax: &'a [(Range<usize>, Color32)],
    /// Byte ranges of `text` painted with `highlight_bg` behind them (intra-line changes).
    pub highlights: &'a [Range<usize>],
    pub highlight_bg: Color32,
    pub italic: bool,
    /// Selected text: a char range of `text`, and whether the selection runs on past the end of the line.
    pub selected: Option<(Range<usize>, bool)>,
    pub selection_bg: Color32,
}

/// Where a line's text was painted, for hit-testing the pointer against it.
pub(crate) struct Painted {
    origin: Pos2,
    galley: Option<std::sync::Arc<egui::Galley>>,
}

impl Painted {
    /// The char index of `text` nearest to screen x `x` (the caret position a click there gives).
    pub(crate) fn col_at(&self, x: f32) -> usize {
        self.galley.as_ref().map_or(0, |g| {
            g.cursor_from_pos(Vec2::new(x - self.origin.x, g.size().y / 2.0)).index.into()
        })
    }
}

/// Layout job for a line of text: syntax colours (`syntax`, byte ranges with their text colour; the rest in
/// `color`) and intra-line change sections painted with `highlight_bg` behind them. Ranges off char boundaries or
/// past the end are clipped, never sliced blindly.
pub(crate) fn text_job(
    text: &str,
    font: &FontId,
    color: Color32,
    syntax: &[(Range<usize>, Color32)],
    highlights: &[Range<usize>],
    highlight_bg: Color32,
    italic: bool,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_rows = 1;
    job.break_on_newline = false;
    // Section boundaries: every start / end of either kind of range.
    let mut cuts: Vec<usize> = vec![0, text.len()];
    for r in syntax.iter().map(|(r, _)| r).chain(highlights) {
        cuts.extend([r.start, r.end]);
    }
    cuts.retain(|&i| i <= text.len() && text.is_char_boundary(i));
    cuts.sort_unstable();
    cuts.dedup();
    for pair in cuts.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        let Some(section) = text.get(start..end) else { continue };
        let covers = |r: &Range<usize>| r.start <= start && end <= r.end;
        let format = TextFormat {
            font_id: font.clone(),
            color: syntax.iter().find(|(r, _)| covers(r)).map_or(color, |(_, c)| *c),
            background: if highlights.iter().any(covers) {
                highlight_bg
            } else {
                Color32::TRANSPARENT
            },
            italics: italic,
            ..Default::default()
        };
        job.append(section, 0.0, format);
    }
    job
}

/// Paints one line into `rect`: background, number columns, sign, then the selection and the text shifted left by
/// `h_offset` (clipped to the text area).
pub(crate) fn paint_line(painter: &Painter, rect: Rect, m: &Metrics, spec: &LineSpec<'_>, h_offset: f32) -> Painted {
    if spec.bg != Color32::TRANSPARENT {
        painter.rect_filled(rect, 0.0, spec.bg);
    }
    let mid = rect.center().y;
    let mut x = rect.left();
    for n in spec.numbers {
        if let Some(n) = n {
            painter.text(
                Pos2::new(x + m.number_width - GUTTER_PAD / 2.0, mid),
                egui::Align2::RIGHT_CENTER,
                n.to_string(),
                m.font.clone(),
                spec.number_color,
            );
        }
        x += m.number_width;
    }
    if !spec.sign.is_empty() {
        painter.text(
            Pos2::new(x + SIGN_WIDTH / 2.0, mid),
            egui::Align2::CENTER_CENTER,
            spec.sign,
            m.font.clone(),
            spec.sign_color,
        );
    }
    x += SIGN_WIDTH;
    let text_rect = Rect::from_min_max(Pos2::new(x, rect.top()), rect.max);
    let text_painter = painter.with_clip_rect(text_rect.intersect(painter.clip_rect()));
    let galley = (!spec.text.is_empty()).then(|| {
        painter.layout_job(text_job(
            spec.text,
            &m.font,
            spec.text_color,
            spec.syntax,
            spec.highlights,
            spec.highlight_bg,
            spec.italic,
        ))
    });
    let height = galley.as_ref().map_or(m.font.size, |g| g.size().y);
    let origin = Pos2::new(x + GUTTER_PAD / 2.0 - h_offset, mid - height / 2.0);
    if let Some((range, past_end)) = &spec.selected {
        let x_of = |col: usize| {
            galley
                .as_ref()
                .map_or(origin.x, |g| origin.x + g.pos_from_cursor(egui::text::CCursor::new(col)).left())
        };
        let left = x_of(range.start);
        let mut right = x_of(range.end);
        if *past_end {
            right += m.char_width / 2.0;
        }
        if right > left {
            text_painter.rect_filled(Rect::from_x_y_ranges(left..=right, rect.y_range()), 0.0, spec.selection_bg);
        }
    }
    if let Some(galley) = &galley {
        text_painter.galley(origin, galley.clone(), spec.text_color);
    }
    Painted { origin, galley }
}

/// Vertically virtualised rows of [`ROW_HEIGHT`]. `add_row(ui, index, rect)` paints row `index` into `rect`
/// (full width of the area). `scroll_to` jumps so that row is at the top (this frame). Returns the index of the
/// first visible row.
pub(crate) fn virtual_rows(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    total: usize,
    scroll_to: Option<usize>,
    mut add_row: impl FnMut(&mut Ui, usize, Rect),
) -> usize {
    let mut area = egui::ScrollArea::vertical()
        .id_salt(id_salt)
        .auto_shrink([false, false])
        .content_margin(egui::Margin::ZERO);
    if let Some(row) = scroll_to {
        area = area.vertical_scroll_offset(row as f32 * ROW_HEIGHT);
    }
    let out = ui
        .scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            area.show_rows(ui, ROW_HEIGHT, total, |ui, range| {
                for i in range {
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::hover());
                    add_row(ui, i, rect);
                }
            })
        })
        .inner;
    (out.state.offset.y / ROW_HEIGHT).floor().max(0.0) as usize
}

/// Applies horizontal wheel / trackpad input over `rect` to `offset`, clamped to `0..=max`.
pub(crate) fn scroll_horizontally(ui: &Ui, rect: Rect, offset: &mut f32, max: f32) {
    if ui.rect_contains_pointer(rect) {
        let dx = ui.input(|i| {
            let d = i.smooth_scroll_delta;
            if i.modifiers.shift && d.x == 0.0 { d.y } else { d.x }
        });
        *offset -= dx;
    }
    *offset = offset.clamp(0.0, max.max(0.0));
}

/// A thin horizontal scroll bar in `rect` for content `content` px wide shown in `view` px. Drag or click to
/// move `offset`. Draws nothing when everything fits.
pub(crate) fn h_scrollbar(ui: &mut Ui, rect: Rect, offset: &mut f32, content: f32, view: f32, track: Color32, thumb: Color32) {
    let max = (content - view).max(0.0);
    if max <= 0.0 || rect.width() <= 0.0 {
        *offset = 0.0;
        return;
    }
    let response = ui.interact(rect, ui.id().with("h_scrollbar"), Sense::click_and_drag());
    let thumb_w = (rect.width() * view / content).clamp(24.0, rect.width());
    let travel = (rect.width() - thumb_w).max(1.0);
    if let Some(pointer) = response.interact_pointer_pos() {
        let t = ((pointer.x - rect.left() - thumb_w / 2.0) / travel).clamp(0.0, 1.0);
        *offset = t * max;
    }
    *offset = offset.clamp(0.0, max);
    let x = rect.left() + travel * (*offset / max);
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, track);
    painter.rect_filled(
        Rect::from_min_size(Pos2::new(x, rect.top() + 1.0), Vec2::new(thumb_w, rect.height() - 2.0)),
        3.0,
        thumb,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_job_splits_highlight_sections() {
        let font = FontId::monospace(12.0);
        let job = text_job("let a = 1;", &font, Color32::WHITE, &[], &[4..5, 8..9], Color32::RED, false);
        let parts: Vec<(&str, bool)> = job
            .sections
            .iter()
            .map(|s| {
                (
                    &job.text[s.byte_range.start.0..s.byte_range.end.0],
                    s.format.background == Color32::RED,
                )
            })
            .collect();
        assert_eq!(parts, [("let ", false), ("a", true), (" = ", false), ("1", true), (";", false)]);
        // Out-of-range / overlapping ranges are skipped, never sliced blindly.
        let job = text_job("abc", &font, Color32::WHITE, &[], &[1..2, 0..1, 2..10], Color32::RED, false);
        assert_eq!(job.text, "abc");
    }
}
