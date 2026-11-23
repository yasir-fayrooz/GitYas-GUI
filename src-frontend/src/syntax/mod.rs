//! Syntax highlighting for the diff viewer and the merge editor: tree-sitter grammars with
//! their bundled highlight queries (captures such as `keyword`, `string.escape`, `function.method`), including
//! injections (`<script>` in HTML, fenced code in Markdown, …), mapped onto a fixed set of [`Highlight`]
//! categories that the theme colours with One Dark / One Light syntax palettes ([`Palette::syntax`]).
//!
//! Languages without a bundled grammar ([`languages`]) fall back to bat's Sublime syntaxes (`two-face` on
//! `syntect`, [`fallback`]), whose scopes map onto the same categories, so ~250 languages highlight out of the box.
//! The language comes from the file name (then the first line, e.g. a shebang); nothing is configurable.
//!
//! [`Highlights::compute`] parses a whole file (so multi-line strings and comments are right on every line) and is
//! meant to run off the UI thread where possible; rows then look up their line's spans by byte range.

mod fallback;
mod languages;

use std::ops::Range;
use std::path::Path;

use egui::Color32;

use crate::theme::Palette;

/// Files larger than this are shown without highlighting (parsing cost grows with size, and the diff viewer
/// already caps what it loads).
const MAX_BYTES: usize = 2 * 1024 * 1024;

/// Highlight categories, named after common editor theme `syntax` keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Highlight {
    Attribute,
    Boolean,
    Comment,
    CommentDoc,
    Constant,
    Constructor,
    Embedded,
    Emphasis,
    EmphasisStrong,
    Enum,
    Function,
    Keyword,
    Label,
    LinkText,
    LinkUri,
    Namespace,
    Number,
    Operator,
    Preproc,
    Property,
    Punctuation,
    PunctuationBracket,
    PunctuationDelimiter,
    PunctuationListMarker,
    PunctuationSpecial,
    String,
    StringEscape,
    StringRegex,
    StringSpecial,
    Tag,
    TextLiteral,
    Title,
    Type,
    Variable,
    VariableParameter,
    VariableSpecial,
    Variant,
    DiffPlus,
    DiffMinus,
}

/// A highlighted byte range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub highlight: Highlight,
}

/// Highlighted byte ranges of one text: sorted, non-overlapping, adjacent equal spans merged. Unhighlighted text
/// has no span.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Highlights {
    spans: Vec<Span>,
}

impl Highlights {
    /// Highlights `text`, the content of the file at `path` (only its name is used). Empty when the language is
    /// unknown or the text is too large.
    pub fn compute(path: &Path, text: &str) -> Self {
        if text.is_empty() || text.len() > MAX_BYTES {
            return Self::default();
        }
        let spans = match languages::detect(path) {
            Some(grammar) => languages::highlight(grammar, text),
            None => fallback::highlight(path, text),
        };
        Self {
            spans: spans.unwrap_or_default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Spans overlapping `range`, clipped to it, relative to `range.start`.
    pub fn within(&self, range: Range<usize>) -> impl Iterator<Item = Span> + '_ {
        let first = self.spans.partition_point(|s| s.range.end <= range.start);
        self.spans[first..]
            .iter()
            .take_while(move |s| s.range.start < range.end)
            .map(move |s| Span {
                range: s.range.start.max(range.start) - range.start..s.range.end.min(range.end) - range.start,
                highlight: s.highlight,
            })
    }

    /// Text colours for `range` (relative to its start), dropping categories drawn in the plain text colour.
    pub fn colors(&self, range: Range<usize>, p: &Palette) -> Vec<(Range<usize>, Color32)> {
        self.within(range).filter_map(|s| Some((s.range, p.syntax(s.highlight)?))).collect()
    }
}

/// [`Highlights`] of a whole text plus its line starts, to look lines up by number (diff sides, merge panes).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HighlightedText {
    highlights: Highlights,
    line_starts: Vec<usize>,
}

impl HighlightedText {
    pub fn new(path: &Path, text: &str) -> Self {
        let highlights = Highlights::compute(path, text);
        if highlights.is_empty() {
            return Self::default();
        }
        let mut line_starts = vec![0];
        line_starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Self { highlights, line_starts }
    }

    /// Text colours for `range` of the text.
    pub fn colors(&self, range: Range<usize>, p: &Palette) -> Vec<(Range<usize>, Color32)> {
        self.highlights.colors(range, p)
    }

    /// Text colours for line `index` (0-based) of `text`, whose content (without terminator) is `line`. Empty when
    /// `text` doesn't hold `line` there (the row and the text disagree, so the spans would be misplaced).
    pub fn line_colors(&self, text: &str, index: usize, line: &str, p: &Palette) -> Vec<(Range<usize>, Color32)> {
        let Some(&start) = self.line_starts.get(index) else {
            return Vec::new();
        };
        let range = start..start + line.len();
        if text.get(range.clone()) != Some(line) {
            return Vec::new();
        }
        self.colors(range, p)
    }
}

/// Appends `range` as `highlight`, merging with the previous span when they touch and match.
fn push(spans: &mut Vec<Span>, range: Range<usize>, highlight: Highlight) {
    if range.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.highlight == highlight
        && last.range.end == range.start
    {
        last.range.end = range.end;
        return;
    }
    spans.push(Span { range, highlight });
}

#[cfg(test)]
mod tests;
