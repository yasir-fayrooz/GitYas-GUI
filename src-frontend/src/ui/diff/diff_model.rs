//! Pure, UI-free row model of the diff viewer (plan WP13): `FileDiff` + mode → rows.
//!
//! All three modes are derived from the **same** `FileDiff` the hunk commands use (bug-log #9: the old viewer let
//! Monaco recompute its own diff, which could disagree with the stageable hunks):
//!
//! * [`hunk_rows`] (Hunk mode): one header row per hunk followed by its lines, with a blank [`RowKind::Spacer`]
//!   row between hunks.
//! * [`inline_rows`] (Inline mode): the whole file; unchanged gaps between hunks come from `old_text`, the hunks
//!   from `Hunk::lines`. Long gaps can be folded into one [`RowKind::Fold`] row.
//! * [`split_rows`] (Split mode): two aligned columns; each run of deleted / added lines is paired row by row and
//!   the shorter side is padded with filler cells.
//!
//! Rows never copy text: a [`TextRef`] points into the diff, resolved with [`text`]. Out-of-range hunk positions
//! (an inconsistent diff) are clamped, never indexed blindly.

use std::collections::BTreeSet;
use std::ops::Range;

use gityas_core::diff::{FileDiff, LineOrigin};

/// Where a row's text lives in the `FileDiff`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextRef {
    None,
    /// `hunks[i].header`, without the section heading git appends after the closing `@@`.
    Header(usize),
    /// `hunks[hunk].lines[line].text`.
    Hunk {
        hunk: usize,
        line: usize,
    },
    /// A byte range of `old_text` (an unchanged line outside the hunks; identical on the new side).
    Old(Range<usize>),
}

/// Kind of a unified (Hunk / Inline) row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// `@@ -a,b +c,d @@` (Hunk mode only); carries the hunk actions.
    HunkHeader,
    /// Blank gap between two hunks (Hunk mode only).
    Spacer,
    Context,
    Add,
    Del,
    /// `\ No newline at end of file`.
    NoNewline,
    /// `lines` unchanged lines of gap `gap` hidden (Inline / Split with folding); click to expand.
    Fold {
        gap: usize,
        lines: usize,
    },
}

impl RowKind {
    fn of(origin: LineOrigin) -> Self {
        match origin {
            LineOrigin::Context => RowKind::Context,
            LineOrigin::Add => RowKind::Add,
            LineOrigin::Del => RowKind::Del,
            LineOrigin::NoNewlineAtEof => RowKind::NoNewline,
        }
    }

    /// Unified-diff sign shown in the gutter.
    pub fn sign(self) -> &'static str {
        match self {
            RowKind::Add => "+",
            RowKind::Del => "−",
            _ => "",
        }
    }
}

/// One unified row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: TextRef,
    /// The hunk this row belongs to (`None` for unchanged gap lines and folds).
    pub hunk: Option<usize>,
}

/// Kind of one side of a split row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Context,
    Del,
    Add,
    NoNewline,
    /// Padding opposite a longer del / add run.
    Filler,
    Fold {
        gap: usize,
        lines: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub kind: CellKind,
    pub no: Option<u32>,
    pub text: TextRef,
}

impl Cell {
    fn filler() -> Self {
        Cell {
            kind: CellKind::Filler,
            no: None,
            text: TextRef::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitRow {
    pub left: Cell,
    pub right: Cell,
    pub hunk: Option<usize>,
}

/// Rows plus the row index where each hunk starts (J / K navigation). `hunk_starts[i]` is hunk `i`'s first row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rows<R> {
    pub rows: Vec<R>,
    pub hunk_starts: Vec<usize>,
    /// Longest line in characters (horizontal scroll extent).
    pub max_chars: usize,
}

impl<R> Default for Rows<R> {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            hunk_starts: Vec::new(),
            max_chars: 0,
        }
    }
}

/// Folding of unchanged gaps (Inline / Split): gaps of at least `min_lines` lines collapse to one fold row unless
/// their index is in `expanded`. Gap `0` is before the first hunk, gap `i` after hunk `i - 1`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Folding {
    pub min_lines: Option<usize>,
    pub expanded: BTreeSet<usize>,
}

impl Folding {
    #[cfg(test)]
    pub fn off() -> Self {
        Self::default()
    }

    fn folds(&self, gap: usize, lines: usize) -> bool {
        self.min_lines.is_some_and(|min| lines >= min.max(1)) && !self.expanded.contains(&gap)
    }
}

/// Resolves a row's text.
pub fn text<'a>(diff: &'a FileDiff, text: &TextRef) -> &'a str {
    match text {
        TextRef::None => "",
        TextRef::Header(h) => diff.hunks.get(*h).map_or("", |h| header_label(&h.header)),
        TextRef::Hunk { hunk, line } => diff
            .hunks
            .get(*hunk)
            .and_then(|h| h.lines.get(*line))
            .map_or("", |l| l.text.as_str()),
        TextRef::Old(range) => diff.old_text.get(range.clone()).unwrap_or(""),
    }
}

/// The `@@ -a,b +c,d @@` part of a hunk header, without the section heading git appends after it
/// (`@@ -1,5 +1,5 @@ fn foo() {`).
pub fn header_label(header: &str) -> &str {
    header
        .strip_prefix("@@")
        .and_then(|rest| rest.find("@@"))
        .map_or(header.trim_end(), |end| &header[..end + 4])
}

/// Byte ranges of the lines of `text`, without `\n` / `\r\n` terminators. A trailing terminator does not start
/// another line; `""` has no lines.
pub fn line_ranges(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            let end = if i > start && bytes.get(i - 1) == Some(&b'\r') { i - 1 } else { i };
            out.push(start..end);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        let end = if bytes.last() == Some(&b'\r') {
            bytes.len() - 1
        } else {
            bytes.len()
        };
        out.push(start..end.max(start));
    }
    out
}

fn chars(diff: &FileDiff, t: &TextRef) -> usize {
    text(diff, t).chars().count()
}

/// Hunk mode.
pub fn hunk_rows(diff: &FileDiff) -> Rows<Row> {
    let mut out = Rows::default();
    for (h, hunk) in diff.hunks.iter().enumerate() {
        if h > 0 {
            out.rows.push(Row {
                kind: RowKind::Spacer,
                old_no: None,
                new_no: None,
                text: TextRef::None,
                hunk: None,
            });
        }
        out.hunk_starts.push(out.rows.len());
        out.max_chars = out.max_chars.max(header_label(&hunk.header).chars().count());
        out.rows.push(Row {
            kind: RowKind::HunkHeader,
            old_no: None,
            new_no: None,
            text: TextRef::Header(h),
            hunk: Some(h),
        });
        push_hunk_lines(diff, h, &mut out);
    }
    out
}

fn push_hunk_lines(diff: &FileDiff, h: usize, out: &mut Rows<Row>) {
    let Some(hunk) = diff.hunks.get(h) else { return };
    for (l, line) in hunk.lines.iter().enumerate() {
        out.max_chars = out.max_chars.max(line.text.chars().count());
        out.rows.push(Row {
            kind: RowKind::of(line.origin),
            old_no: line.old_lineno,
            new_no: line.new_lineno,
            text: TextRef::Hunk { hunk: h, line: l },
            hunk: Some(h),
        });
    }
}

/// One unchanged gap of `old_text`: old line indices (0-based) and the new-minus-old line offset.
struct Gap {
    index: usize,
    lines: Range<usize>,
    offset: i64,
}

/// Walks the file: `gap(..)` for each unchanged stretch (possibly empty), `hunk(h)` for each hunk, in order.
fn walk(diff: &FileDiff, old_lines: usize, mut gap: impl FnMut(Gap), mut hunk: impl FnMut(usize)) {
    let mut pos = 0usize;
    let mut offset = 0i64;
    for (h, hk) in diff.hunks.iter().enumerate() {
        // Unified-diff convention: a zero-length old range starts *after* line `old_start`.
        let start = if hk.id.old_lines == 0 {
            hk.id.old_start
        } else {
            hk.id.old_start.saturating_sub(1)
        };
        let start = usize::try_from(start).unwrap_or(usize::MAX).clamp(pos, old_lines.max(pos));
        gap(Gap {
            index: h,
            lines: pos..start,
            offset,
        });
        hunk(h);
        let consumed = usize::try_from(hk.id.old_lines).unwrap_or(usize::MAX);
        pos = start.saturating_add(consumed).min(old_lines.max(start));
        offset += i64::from(hk.id.new_lines) - i64::from(hk.id.old_lines);
    }
    gap(Gap {
        index: diff.hunks.len(),
        lines: pos..old_lines.max(pos),
        offset,
    });
}

fn lineno(index: usize, offset: i64) -> Option<u32> {
    let n = i64::try_from(index).ok()?.checked_add(1)?.checked_add(offset)?;
    u32::try_from(n).ok().filter(|n| *n > 0)
}

/// Inline mode (full file, unified).
pub fn inline_rows(diff: &FileDiff, folding: &Folding) -> Rows<Row> {
    let old = line_ranges(&diff.old_text);
    let out = std::cell::RefCell::new(Rows::<Row>::default());
    walk(
        diff,
        old.len(),
        |g| {
            let mut out = out.borrow_mut();
            if g.lines.is_empty() {
                return;
            }
            if folding.folds(g.index, g.lines.len()) {
                out.rows.push(Row {
                    kind: RowKind::Fold {
                        gap: g.index,
                        lines: g.lines.len(),
                    },
                    old_no: None,
                    new_no: None,
                    text: TextRef::None,
                    hunk: None,
                });
                return;
            }
            for i in g.lines {
                let Some(range) = old.get(i) else { break };
                let t = TextRef::Old(range.clone());
                out.max_chars = out.max_chars.max(chars(diff, &t));
                out.rows.push(Row {
                    kind: RowKind::Context,
                    old_no: lineno(i, 0),
                    new_no: lineno(i, g.offset),
                    text: t,
                    hunk: None,
                });
            }
        },
        |h| {
            let mut out = out.borrow_mut();
            let start = out.rows.len();
            out.hunk_starts.push(start);
            push_hunk_lines(diff, h, &mut out);
        },
    );
    out.into_inner()
}

/// Split mode (full file, two aligned columns).
pub fn split_rows(diff: &FileDiff, folding: &Folding) -> Rows<SplitRow> {
    let old = line_ranges(&diff.old_text);
    let out = std::cell::RefCell::new(Rows::<SplitRow>::default());
    walk(
        diff,
        old.len(),
        |g| {
            let mut out = out.borrow_mut();
            if g.lines.is_empty() {
                return;
            }
            if folding.folds(g.index, g.lines.len()) {
                let fold = Cell {
                    kind: CellKind::Fold {
                        gap: g.index,
                        lines: g.lines.len(),
                    },
                    no: None,
                    text: TextRef::None,
                };
                out.rows.push(SplitRow {
                    left: fold.clone(),
                    right: fold,
                    hunk: None,
                });
                return;
            }
            for i in g.lines {
                let Some(range) = old.get(i) else { break };
                let t = TextRef::Old(range.clone());
                out.max_chars = out.max_chars.max(chars(diff, &t));
                out.rows.push(SplitRow {
                    left: Cell {
                        kind: CellKind::Context,
                        no: lineno(i, 0),
                        text: t.clone(),
                    },
                    right: Cell {
                        kind: CellKind::Context,
                        no: lineno(i, g.offset),
                        text: t,
                    },
                    hunk: None,
                });
            }
        },
        |h| {
            let mut out = out.borrow_mut();
            let start = out.rows.len();
            out.hunk_starts.push(start);
            push_split_hunk(diff, h, &mut out);
        },
    );
    out.into_inner()
}

fn push_split_hunk(diff: &FileDiff, h: usize, out: &mut Rows<SplitRow>) {
    let Some(hunk) = diff.hunks.get(h) else { return };
    let mut dels: Vec<Cell> = Vec::new();
    let mut adds: Vec<Cell> = Vec::new();
    // Which side the previous line was on, so a `\ No newline` marker follows its line.
    let mut last = LineOrigin::Context;
    let flush = |dels: &mut Vec<Cell>, adds: &mut Vec<Cell>, out: &mut Rows<SplitRow>| {
        let n = dels.len().max(adds.len());
        let mut d = dels.drain(..);
        let mut a = adds.drain(..);
        for _ in 0..n {
            out.rows.push(SplitRow {
                left: d.next().unwrap_or_else(Cell::filler),
                right: a.next().unwrap_or_else(Cell::filler),
                hunk: Some(h),
            });
        }
    };
    for (l, line) in hunk.lines.iter().enumerate() {
        out.max_chars = out.max_chars.max(line.text.chars().count());
        let t = TextRef::Hunk { hunk: h, line: l };
        match line.origin {
            LineOrigin::Del => dels.push(Cell {
                kind: CellKind::Del,
                no: line.old_lineno,
                text: t,
            }),
            LineOrigin::Add => adds.push(Cell {
                kind: CellKind::Add,
                no: line.new_lineno,
                text: t,
            }),
            LineOrigin::NoNewlineAtEof => {
                let marker = Cell {
                    kind: CellKind::NoNewline,
                    no: None,
                    text: t,
                };
                match last {
                    LineOrigin::Del => dels.push(marker),
                    LineOrigin::Add => adds.push(marker),
                    // After a context line both sides lack the newline; one row spanning both.
                    LineOrigin::Context | LineOrigin::NoNewlineAtEof => {
                        flush(&mut dels, &mut adds, out);
                        out.rows.push(SplitRow {
                            left: marker.clone(),
                            right: marker,
                            hunk: Some(h),
                        });
                    }
                }
            }
            LineOrigin::Context => {
                flush(&mut dels, &mut adds, out);
                out.rows.push(SplitRow {
                    left: Cell {
                        kind: CellKind::Context,
                        no: line.old_lineno,
                        text: t.clone(),
                    },
                    right: Cell {
                        kind: CellKind::Context,
                        no: line.new_lineno,
                        text: t,
                    },
                    hunk: Some(h),
                });
            }
        }
        if line.origin != LineOrigin::NoNewlineAtEof {
            last = line.origin;
        }
    }
    flush(&mut dels, &mut adds, out);
}

/// The hunk to jump to with J / F7 (`forward`) or K / Shift+F7, given each hunk's first row, the first visible
/// row and the hunk jumped to last (`focused`). A focused hunk at or below the top is the reference point;
/// otherwise (nothing focused, or the user scrolled past it) the top row is. Stops at the first / last hunk.
pub fn next_hunk(hunk_starts: &[usize], top: usize, focused: Option<usize>, forward: bool) -> Option<usize> {
    let last = hunk_starts.len().checked_sub(1)?;
    let focused = focused.filter(|h| hunk_starts.get(*h).is_some_and(|s| *s >= top));
    Some(match (focused, forward) {
        (Some(h), true) => (h + 1).min(last),
        (Some(h), false) => h.saturating_sub(1),
        (None, true) => hunk_starts.iter().position(|s| *s >= top).unwrap_or(last),
        (None, false) => hunk_starts.iter().rposition(|s| *s < top).unwrap_or(0),
    })
}

/// "File mode changed 100644 → 100755", for a mode-only change or a typechange.
pub fn mode_change_label(diff: &FileDiff) -> Option<String> {
    let (Some(old), Some(new)) = (diff.old_mode, diff.new_mode) else {
        return None;
    };
    (old != new).then(|| format!("File mode changed {old:o} → {new:o}"))
}

/// Index of the last hunk starting at or before `row` (the hunk the viewer is "in").
#[cfg(test)]
pub fn hunk_at_row(hunk_starts: &[usize], row: usize) -> Option<usize> {
    hunk_starts.iter().rposition(|start| *start <= row)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use gityas_core::diff::{DiffKind, DiffLine, Hunk, HunkRef};

    use super::*;

    /// A FileDiff from unified-diff text: `@@ -a,b +c,d @@` headers and ` `/`+`/`-`/`\` lines, with the line numbers
    /// computed the way libgit2 reports them.
    fn diff(old: &str, new: &str, patch: &str) -> FileDiff {
        let mut hunks: Vec<Hunk> = Vec::new();
        let (mut o, mut n) = (0u32, 0u32);
        for raw in patch.lines() {
            if let Some(rest) = raw.strip_prefix("@@ -") {
                let nums: Vec<u32> = rest
                    .trim_end_matches(" @@")
                    .split([' ', ',', '+'])
                    .filter(|s| !s.is_empty())
                    .map(|s| s.parse().unwrap())
                    .collect();
                let [os, ol, ns, nl] = nums[..] else { panic!("bad header {raw}") };
                o = if ol == 0 { os + 1 } else { os };
                n = if nl == 0 { ns + 1 } else { ns };
                hunks.push(Hunk {
                    id: HunkRef {
                        old_start: os,
                        old_lines: ol,
                        new_start: ns,
                        new_lines: nl,
                        content_hash: hunks.len() as u64,
                    },
                    header: raw.to_string(),
                    lines: vec![],
                });
                continue;
            }
            let (origin, text) = raw.split_at(1);
            let line = match origin {
                " " => {
                    o += 1;
                    n += 1;
                    DiffLine {
                        origin: LineOrigin::Context,
                        old_lineno: Some(o - 1),
                        new_lineno: Some(n - 1),
                        text: text.into(),
                    }
                }
                "-" => {
                    o += 1;
                    DiffLine {
                        origin: LineOrigin::Del,
                        old_lineno: Some(o - 1),
                        new_lineno: None,
                        text: text.into(),
                    }
                }
                "+" => {
                    n += 1;
                    DiffLine {
                        origin: LineOrigin::Add,
                        old_lineno: None,
                        new_lineno: Some(n - 1),
                        text: text.into(),
                    }
                }
                "\\" => DiffLine {
                    origin: LineOrigin::NoNewlineAtEof,
                    old_lineno: None,
                    new_lineno: None,
                    text: "\\ No newline at end of file".into(),
                },
                other => panic!("bad origin {other}"),
            };
            hunks.last_mut().unwrap().lines.push(line);
        }
        for h in &hunks {
            let count = |o: LineOrigin| h.lines.iter().filter(|l| l.origin == o || l.origin == LineOrigin::Context).count() as u32;
            assert_eq!(count(LineOrigin::Del), h.id.old_lines, "old count of {}", h.header);
            assert_eq!(count(LineOrigin::Add), h.id.new_lines, "new count of {}", h.header);
        }
        FileDiff {
            old_text: Arc::from(old),
            new_text: Arc::from(new),
            hunks,
            kind: DiffKind::Text,
            old_mode: Some(0o100_644),
            new_mode: Some(0o100_644),
        }
    }

    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("l{i}\n")).collect()
    }

    /// Unified rows as `sign old new text` strings.
    fn render(d: &FileDiff, rows: &Rows<Row>) -> Vec<String> {
        rows.rows
            .iter()
            .map(|r| {
                let s = match r.kind {
                    RowKind::HunkHeader => "@",
                    RowKind::Spacer => "",
                    RowKind::Context => " ",
                    RowKind::Add => "+",
                    RowKind::Del => "-",
                    RowKind::NoNewline => "\\",
                    RowKind::Fold { .. } => "~",
                };
                let no = |n: Option<u32>| n.map_or("_".to_string(), |n| n.to_string());
                match r.kind {
                    RowKind::Fold { lines, .. } => format!("~ {lines}"),
                    _ => format!("{s} {} {} {}", no(r.old_no), no(r.new_no), text(d, &r.text)),
                }
            })
            .collect()
    }

    fn render_split(d: &FileDiff, rows: &Rows<SplitRow>) -> Vec<String> {
        let cell = |c: &Cell| {
            let k = match c.kind {
                CellKind::Context => " ",
                CellKind::Del => "-",
                CellKind::Add => "+",
                CellKind::NoNewline => "\\",
                CellKind::Filler => ".",
                CellKind::Fold { .. } => "~",
            };
            format!("{k}{}:{}", c.no.map_or("_".into(), |n: u32| n.to_string()), text(d, &c.text))
        };
        rows.rows
            .iter()
            .map(|r| format!("{} | {}", cell(&r.left), cell(&r.right)))
            .collect()
    }

    /// Every line of every hunk appears exactly once (as a `TextRef::Hunk`), in order.
    fn assert_every_hunk_line_once(d: &FileDiff, refs: impl Iterator<Item = TextRef>) {
        let mut seen: Vec<(usize, usize)> = refs
            .filter_map(|t| match t {
                TextRef::Hunk { hunk, line } => Some((hunk, line)),
                _ => None,
            })
            .collect();
        let expected: Vec<(usize, usize)> = d
            .hunks
            .iter()
            .enumerate()
            .flat_map(|(h, hk)| (0..hk.lines.len()).map(move |l| (h, l)))
            .collect();
        let in_order = seen.clone();
        seen.sort();
        seen.dedup();
        assert_eq!(seen, expected, "every hunk line exactly once");
        assert_eq!(in_order.len(), expected.len(), "no duplicates");
    }

    /// Model invariants on any diff (also run on real engine diffs by the UI tests).
    pub(crate) fn check_all_modes(d: &FileDiff) {
        let h = hunk_rows(d);
        assert_every_hunk_line_once(d, h.rows.iter().map(|r| r.text.clone()));
        let i = inline_rows(d, &Folding::off());
        assert_every_hunk_line_once(d, i.rows.iter().map(|r| r.text.clone()));
        let s = split_rows(d, &Folding::off());
        // A context / marker line spanning both columns counts once.
        assert_every_hunk_line_once(
            d,
            s.rows.iter().flat_map(|r| {
                if r.left.text == r.right.text {
                    vec![r.left.text.clone()]
                } else {
                    vec![r.left.text.clone(), r.right.text.clone()]
                }
            }),
        );
        assert_eq!(h.hunk_starts.len(), d.hunks.len());
        assert_eq!(i.hunk_starts.len(), d.hunks.len());
        assert_eq!(s.hunk_starts.len(), d.hunks.len());
        // Inline / Split show every old line and every new line exactly once (no folding).
        let old_lines = line_ranges(&d.old_text).len();
        let new_lines = line_ranges(&d.new_text).len();
        let olds: Vec<u32> = i.rows.iter().filter_map(|r| r.old_no).collect();
        let news: Vec<u32> = i.rows.iter().filter_map(|r| r.new_no).collect();
        assert_eq!(olds, (1..=old_lines as u32).collect::<Vec<_>>(), "inline old numbers");
        assert_eq!(news, (1..=new_lines as u32).collect::<Vec<_>>(), "inline new numbers");
        let lefts: Vec<u32> = s.rows.iter().filter_map(|r| r.left.no).collect();
        let rights: Vec<u32> = s.rows.iter().filter_map(|r| r.right.no).collect();
        assert_eq!(lefts, (1..=old_lines as u32).collect::<Vec<_>>(), "split left numbers");
        assert_eq!(rights, (1..=new_lines as u32).collect::<Vec<_>>(), "split right numbers");
        // Inline new-side text equals new_text line by line.
        let new_ranges = line_ranges(&d.new_text);
        for r in &i.rows {
            if let Some(n) = r.new_no {
                let want = d.new_text.get(new_ranges[n as usize - 1].clone()).unwrap();
                assert_eq!(text(d, &r.text), want, "new line {n}");
            }
        }
    }

    #[test]
    fn line_ranges_handle_terminators() {
        let t = "a\r\nb\nc";
        let lines: Vec<&str> = line_ranges(t).into_iter().map(|r| &t[r]).collect();
        assert_eq!(lines, ["a", "b", "c"]);
        assert!(line_ranges("").is_empty());
        assert_eq!(line_ranges("\n").len(), 1);
        let t = "x\r";
        assert_eq!(line_ranges(t).into_iter().map(|r| &t[r]).collect::<Vec<_>>(), ["x"]);
        let t = "a\n\nb\n";
        assert_eq!(line_ranges(t).into_iter().map(|r| &t[r]).collect::<Vec<_>>(), ["a", "", "b"]);
    }

    #[test]
    fn modified_middle_line() {
        let old = numbered(9);
        let new = old.replace("l5\n", "L5\n");
        let d = diff(&old, &new, "@@ -2,7 +2,7 @@\n l2\n l3\n l4\n-l5\n+L5\n l6\n l7\n l8");
        check_all_modes(&d);
        assert_eq!(
            render(&d, &hunk_rows(&d)),
            [
                "@ _ _ @@ -2,7 +2,7 @@",
                "  2 2 l2",
                "  3 3 l3",
                "  4 4 l4",
                "- 5 _ l5",
                "+ _ 5 L5",
                "  6 6 l6",
                "  7 7 l7",
                "  8 8 l8"
            ]
        );
        let inline = render(&d, &inline_rows(&d, &Folding::off()));
        assert_eq!(inline.first().map(String::as_str), Some("  1 1 l1"));
        assert_eq!(inline.last().map(String::as_str), Some("  9 9 l9"));
        assert_eq!(inline.len(), 10);
        let split = render_split(&d, &split_rows(&d, &Folding::off()));
        assert_eq!(split.len(), 9);
        assert_eq!(split[4], "-5:l5 | +5:L5");
    }

    #[test]
    fn pure_add_of_a_new_file() {
        let d = diff("", "a\nb\n", "@@ -0,0 +1,2 @@\n+a\n+b");
        check_all_modes(&d);
        assert_eq!(render(&d, &inline_rows(&d, &Folding::off())), ["+ _ 1 a", "+ _ 2 b"]);
        assert_eq!(render_split(&d, &split_rows(&d, &Folding::off())), ["._: | +1:a", "._: | +2:b"]);
    }

    #[test]
    fn pure_delete_of_a_file() {
        let d = diff("a\nb\n", "", "@@ -1,2 +0,0 @@\n-a\n-b");
        check_all_modes(&d);
        assert_eq!(render_split(&d, &split_rows(&d, &Folding::off())), ["-1:a | ._:", "-2:b | ._:"]);
    }

    #[test]
    fn insertion_and_deletion_in_the_middle() {
        let old = numbered(20);
        let new = old.replace("l5\n", "l5\nnew\n").replace("l15\n", "");
        let d = diff(
            &old,
            &new,
            "@@ -3,6 +3,7 @@\n l3\n l4\n l5\n+new\n l6\n l7\n l8\n@@ -12,7 +13,6 @@\n l12\n l13\n l14\n-l15\n l16\n l17\n l18",
        );
        check_all_modes(&d);
        let inline = render(&d, &inline_rows(&d, &Folding::off()));
        assert_eq!(inline[9], "  9 10 l9", "gap after an insertion is offset by one");
        assert_eq!(inline.last().map(String::as_str), Some("  20 20 l20"));
    }

    #[test]
    fn multiple_hunks_with_gaps_and_folding() {
        let old = numbered(40);
        let new = old.replace("l2\n", "L2\n").replace("l39\n", "L39\n");
        let d = diff(
            &old,
            &new,
            "@@ -1,5 +1,5 @@\n l1\n-l2\n+L2\n l3\n l4\n l5\n@@ -36,5 +36,5 @@\n l36\n l37\n l38\n-l39\n+L39\n l40",
        );
        check_all_modes(&d);
        let h = hunk_rows(&d);
        assert_eq!(h.hunk_starts, [0, 8], "a spacer row separates the hunks");
        assert_eq!(h.rows[7].kind, RowKind::Spacer);
        let i = inline_rows(&d, &Folding::off());
        assert_eq!(i.hunk_starts, [0, 6 + 30]);
        // Gap between the hunks (l6..l35, 30 lines) folds; the empty gaps at the ends don't.
        let folding = Folding {
            min_lines: Some(10),
            expanded: BTreeSet::new(),
        };
        let folded = inline_rows(&d, &folding);
        assert_eq!(render(&d, &folded)[6], "~ 30");
        assert_eq!(folded.rows.len(), 6 + 1 + 6);
        assert_eq!(folded.hunk_starts, [0, 7]);
        let split = split_rows(&d, &folding);
        assert!(matches!(split.rows[5].left.kind, CellKind::Fold { gap: 1, lines: 30 }));
        // Expanding gap 1 restores the full rows.
        let expanded = Folding {
            min_lines: Some(10),
            expanded: [1].into(),
        };
        assert_eq!(inline_rows(&d, &expanded), i);
    }

    #[test]
    fn hunk_at_start_and_end_of_file() {
        let old = "a\nb\nc\n";
        let new = "A\nb\nC\n";
        let d = diff(old, new, "@@ -1,3 +1,3 @@\n-a\n+A\n b\n-c\n+C");
        check_all_modes(&d);
        assert_eq!(
            render_split(&d, &split_rows(&d, &Folding::off())),
            ["-1:a | +1:A", " 2:b |  2:b", "-3:c | +3:C"]
        );
    }

    #[test]
    fn no_trailing_newline_markers() {
        // "a\nb" -> "a\nc\nd": both sides lack the final newline.
        let d = diff(
            "a\nb",
            "a\nc\nd",
            "@@ -1,2 +1,3 @@\n a\n-b\n\\ No newline at end of file\n+c\n+d\n\\ No newline at end of file",
        );
        check_all_modes(&d);
        let split = render_split(&d, &split_rows(&d, &Folding::off()));
        assert_eq!(
            split,
            [
                " 1:a |  1:a",
                "-2:b | +2:c",
                "\\_:\\ No newline at end of file | +3:d",
                "._: | \\_:\\ No newline at end of file"
            ]
        );
        // Context line without newline at the end: one marker row across both columns.
        let d = diff("x\ny", "X\ny", "@@ -1,2 +1,2 @@\n-x\n+X\n y\n\\ No newline at end of file");
        check_all_modes(&d);
        let split = split_rows(&d, &Folding::off());
        let last = split.rows.last().unwrap();
        assert!(last.left.kind == CellKind::NoNewline && last.right.kind == CellKind::NoNewline);
    }

    #[test]
    fn crlf_lines_are_shown_without_terminators() {
        let old = "a\r\nb\r\nc\r\nd\r\ne\r\nf\r\n";
        let new = "a\r\nb\r\nc\r\nD\r\ne\r\nf\r\n";
        // libgit2 strips the terminator in DiffLine::text too.
        let d = diff(old, new, "@@ -2,5 +2,5 @@\n b\n c\n-d\n+D\n e\n f");
        check_all_modes(&d);
        let inline = render(&d, &inline_rows(&d, &Folding::off()));
        assert_eq!(inline[0], "  1 1 a");
        assert!(inline.iter().all(|l| !l.contains('\r')));
    }

    #[test]
    fn empty_file_and_mode_only_change() {
        let mut d = diff("", "", "");
        assert!(hunk_rows(&d).rows.is_empty());
        assert!(inline_rows(&d, &Folding::off()).rows.is_empty());
        assert!(split_rows(&d, &Folding::off()).rows.is_empty());
        assert_eq!(mode_change_label(&d), None);
        d.new_mode = Some(0o100_755);
        assert_eq!(mode_change_label(&d).as_deref(), Some("File mode changed 100644 → 100755"));
        // Mode-only change of a non-empty file: no hunks; Inline would show the whole file as context, which the
        // view does not use (it shows only the mode line), but the model stays consistent.
        let mut d = diff("a\n", "a\n", "");
        d.new_mode = Some(0o100_755);
        check_all_modes(&d);
        assert!(hunk_rows(&d).rows.is_empty());
        // Empty file added: no hunks, no rows; mode line absent (no old side).
        let mut added = diff("", "", "");
        added.old_mode = None;
        assert_eq!(mode_change_label(&added), None);
    }

    #[test]
    fn hunk_header_drops_the_section_heading() {
        assert_eq!(header_label("@@ -3,6 +3,7 @@ fn main() {"), "@@ -3,6 +3,7 @@");
        assert_eq!(header_label("@@ -1 +1 @@"), "@@ -1 +1 @@");
        assert_eq!(header_label("@@ -1 +1 @@ a @@ b"), "@@ -1 +1 @@");
        assert_eq!(header_label("garbage "), "garbage");
    }

    #[test]
    fn renamed_file_with_changes_uses_the_same_rows() {
        // Renames only change the header; rows come from the hunks as usual.
        let old = numbered(5);
        let new = old.replace("l3\n", "L3\n");
        let d = diff(&old, &new, "@@ -1,5 +1,5 @@\n l1\n l2\n-l3\n+L3\n l4\n l5");
        check_all_modes(&d);
        // Pure rename (100 % similar): no hunks.
        let pure = diff(&old, &old, "");
        check_all_modes(&pure);
        assert!(hunk_rows(&pure).rows.is_empty());
    }

    #[test]
    fn unequal_runs_pad_with_filler() {
        let d = diff("a\nb\nc\nz\n", "X\nz\n", "@@ -1,4 +1,2 @@\n-a\n-b\n-c\n+X\n z");
        check_all_modes(&d);
        assert_eq!(
            render_split(&d, &split_rows(&d, &Folding::off())),
            ["-1:a | +1:X", "-2:b | ._:", "-3:c | ._:", " 4:z |  2:z"]
        );
    }

    #[test]
    fn inconsistent_hunks_are_clamped() {
        // Hunk claims lines past the end of old_text: no panic, all hunk lines still shown once.
        let d = diff("a\n", "a\nb\n", "@@ -5,1 +5,2 @@\n a\n+b");
        let i = inline_rows(&d, &Folding::off());
        assert_every_hunk_line_once(&d, i.rows.iter().map(|r| r.text.clone()));
        let s = split_rows(&d, &Folding::off());
        assert!(!s.rows.is_empty());
        assert_eq!(text(&d, &TextRef::Old(100..200)), "");
        assert_eq!(text(&d, &TextRef::Hunk { hunk: 9, line: 0 }), "");
    }

    #[test]
    fn navigation() {
        let starts = [0, 7, 20];
        assert_eq!(next_hunk(&[], 0, None, true), None);
        assert_eq!(next_hunk(&starts, 0, None, true), Some(0));
        assert_eq!(next_hunk(&starts, 0, Some(0), true), Some(1));
        assert_eq!(next_hunk(&starts, 7, Some(1), true), Some(2));
        assert_eq!(next_hunk(&starts, 15, Some(2), true), Some(2), "stops at the last");
        assert_eq!(next_hunk(&starts, 15, Some(2), false), Some(1));
        assert_eq!(next_hunk(&starts, 0, Some(0), false), Some(0), "stops at the first");
        // Scrolled past the focused hunk with the mouse: the top row decides.
        assert_eq!(next_hunk(&starts, 10, Some(0), true), Some(2));
        assert_eq!(next_hunk(&starts, 10, Some(0), false), Some(1));
        assert_eq!(next_hunk(&starts, 25, None, true), Some(2));
        assert_eq!(next_hunk(&[5], 0, None, false), Some(0));
        assert_eq!(hunk_at_row(&[0, 7, 20], 0), Some(0));
        assert_eq!(hunk_at_row(&[0, 7, 20], 8), Some(1));
        assert_eq!(hunk_at_row(&[3, 7], 1), None);
        assert_eq!(hunk_at_row(&[], 5), None);
    }
}
