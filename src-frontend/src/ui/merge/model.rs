//! Pure, UI-free merge-editor model: per-chunk, per-line choices → output text, applying a choice to an edited buffer,
//! keeping chunk positions in sync with hand edits, undo / redo, navigation and line mapping between panes.
//!
//! Line model (same as `gityas_git::merge`): a line keeps its terminator (`split_inclusive('\n')`), the last line
//! may have none; ranges are 0-based, end-exclusive line ranges. The output starts from
//! [`MergeConflict::merged`], where every conflict region holds the **base** lines.

use std::ops::Range;

use gityas_core::models::MergeConflict;

/// Whole-side preset for a chunk (the chunk spine and the pane header's "Use all"). Neither = the base lines (the template);
/// both = ours then theirs. Per-line selections are a [`ChunkSel`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Choice {
    pub ours: bool,
    pub theirs: bool,
}

impl Choice {
    pub const BASE: Choice = Choice {
        ours: false,
        theirs: false,
    };
    #[cfg(test)]
    pub const OURS: Choice = Choice { ours: true, theirs: false };
    #[cfg(test)]
    pub const THEIRS: Choice = Choice { ours: false, theirs: true };
    #[cfg(test)]
    pub const BOTH: Choice = Choice { ours: true, theirs: true };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Ours,
    Theirs,
}

/// How much of a side a chunk spine / pane header uses: every line, some lines, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tri {
    None,
    Some,
    All,
}

/// Which lines of one side of a chunk the output takes (indices relative to the chunk's lines on that side).
/// A side with no lines in the chunk (it deleted them) can still be *taken*: the chunk then contributes nothing
/// from it instead of the base lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SideSel {
    lines: Vec<bool>,
    /// Only meaningful when `lines` is empty.
    empty_taken: bool,
}

impl SideSel {
    fn uniform(len: usize, on: bool) -> Self {
        Self {
            lines: vec![on; len],
            empty_taken: on,
        }
    }

    /// The side contributes to the output (at least one line, or an empty side that was taken).
    pub fn is_taken(&self) -> bool {
        if self.lines.is_empty() {
            self.empty_taken
        } else {
            self.lines.iter().any(|l| *l)
        }
    }

    pub fn is_all(&self) -> bool {
        if self.lines.is_empty() {
            self.empty_taken
        } else {
            self.lines.iter().all(|l| *l)
        }
    }

    pub fn state(&self) -> Tri {
        match (self.is_taken(), self.is_all()) {
            (_, true) => Tri::All,
            (true, false) => Tri::Some,
            (false, _) => Tri::None,
        }
    }

    /// Line `k` (relative to the chunk) is taken.
    pub fn line(&self, k: usize) -> bool {
        self.lines.get(k).copied().unwrap_or(false)
    }

    fn set_all(&mut self, on: bool) {
        self.lines.iter_mut().for_each(|l| *l = on);
        self.empty_taken = on;
    }

    fn toggle_line(&mut self, k: usize) -> bool {
        match self.lines.get_mut(k) {
            Some(l) => {
                *l = !*l;
                true
            }
            None => false,
        }
    }
}

/// A chunk's per-line selection on both sides. Output = the taken lines in the order they were taken (as the
/// Tauri editor did); nothing taken = the base lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChunkSel {
    pub ours: SideSel,
    pub theirs: SideSel,
    /// The taken lines in output order: a line taken later goes below the ones already taken.
    order: Vec<(Side, usize)>,
}

impl ChunkSel {
    fn from_choice(choice: Choice, ours_len: usize, theirs_len: usize) -> Self {
        let mut sel = Self {
            ours: SideSel::uniform(ours_len, false),
            theirs: SideSel::uniform(theirs_len, false),
            order: Vec::new(),
        };
        sel.set_side_all(Side::Ours, choice.ours);
        sel.set_side_all(Side::Theirs, choice.theirs);
        sel
    }

    /// Takes (`on`) or drops every line of `side`. Taking moves the whole side below the lines already taken.
    fn set_side_all(&mut self, side: Side, on: bool) {
        self.order.retain(|(s, _)| *s != side);
        let chosen = self.side_mut(side);
        chosen.set_all(on);
        if on {
            let n = chosen.lines.len();
            self.order.extend((0..n).map(|k| (side, k)));
        }
    }

    /// Takes or drops line `k` of `side`; a taken line goes below the lines already taken. False if out of range.
    fn toggle_line(&mut self, side: Side, k: usize) -> bool {
        if !self.side_mut(side).toggle_line(k) {
            return false;
        }
        if self.side(side).line(k) {
            self.order.push((side, k));
        } else {
            self.order.retain(|e| *e != (side, k));
        }
        true
    }

    /// Drops every line of both sides (nothing taken).
    fn clear(&mut self) {
        self.ours.set_all(false);
        self.theirs.set_all(false);
        self.order.clear();
    }

    /// The taken lines in output order.
    pub fn order(&self) -> &[(Side, usize)] {
        &self.order
    }

    pub fn side(&self, side: Side) -> &SideSel {
        match side {
            Side::Ours => &self.ours,
            Side::Theirs => &self.theirs,
        }
    }

    fn side_mut(&mut self, side: Side) -> &mut SideSel {
        match side {
            Side::Ours => &mut self.ours,
            Side::Theirs => &mut self.theirs,
        }
    }

    pub fn is_taken(&self) -> bool {
        self.ours.is_taken() || self.theirs.is_taken()
    }

    #[cfg(test)]
    /// The whole-side preset this selection equals, if every side is all-or-nothing.
    pub fn preset(&self) -> Option<Choice> {
        let whole = |s: &SideSel| matches!(s.state(), Tri::All | Tri::None);
        (whole(&self.ours) && whole(&self.theirs)).then(|| Choice {
            ours: self.ours.is_all(),
            theirs: self.theirs.is_all(),
        })
    }
}

/// The three panes of the editor (for chunk lookup / scroll sync).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Ours,
    Theirs,
    Output,
}

/// One conflict chunk as it currently sits in the output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkState {
    /// Lines of the output buffer that belong to the chunk.
    pub range: Range<usize>,
    pub sel: ChunkSel,
    /// Changed by hand since its last choice.
    pub edited: bool,
    /// Hand-edited text kept above the taken lines: picking a line or side in an edited chunk adds it below the
    /// edit instead of replacing it.
    pub kept: String,
}

impl ChunkState {
    /// The user made a decision (took a side or edited it).
    pub fn is_decided(&self) -> bool {
        self.edited || self.sel.is_taken() || !self.kept.is_empty()
    }

    /// Lines at the chunk's start that are kept hand edits rather than taken lines.
    pub fn kept_lines(&self) -> usize {
        count_lines(&self.kept)
    }
}

/// Byte offsets of line starts (`starts.len() == lines + 1`, last entry = text length).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i + 1));
        if starts.last() != Some(&text.len()) {
            starts.push(text.len());
        }
        Self { starts }
    }

    pub fn lines(&self) -> usize {
        self.starts.len().saturating_sub(1)
    }

    /// Byte range of `lines`; `None` if out of bounds.
    pub fn byte_range(&self, lines: Range<usize>) -> Option<Range<usize>> {
        if lines.start > lines.end {
            return None;
        }
        Some(*self.starts.get(lines.start)?..*self.starts.get(lines.end)?)
    }

    pub fn slice<'a>(&self, text: &'a str, lines: Range<usize>) -> Option<&'a str> {
        text.get(self.byte_range(lines)?)
    }

    /// Line `i` including its terminator.
    pub fn line<'a>(&self, text: &'a str, i: usize) -> Option<&'a str> {
        self.slice(text, i..i + 1)
    }
}

/// Number of lines in `text` (the `split_inclusive('\n')` count).
pub fn count_lines(text: &str) -> usize {
    LineIndex::new(text).lines()
}

/// A line that looks like a git conflict marker (`<<<<<<< ours` / `>>>>>>> theirs`).
pub fn is_marker_line(line: &str) -> bool {
    let line = line.trim_end_matches(['\n', '\r']);
    ["<<<<<<<", ">>>>>>>"]
        .iter()
        .any(|m| line == *m || line.strip_prefix(m).is_some_and(|rest| rest.starts_with(' ')))
}

/// The file's dominant line ending (used for separators the editor inserts).
pub fn dominant_eol(texts: &[&str]) -> &'static str {
    let (mut crlf, mut lf) = (0usize, 0usize);
    for text in texts {
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                if i > 0 && text.as_bytes().get(i - 1) == Some(&b'\r') {
                    crlf += 1;
                } else {
                    lf += 1;
                }
            }
        }
    }
    if crlf > lf { "\r\n" } else { "\n" }
}

/// Why a [`MergeConflict`] can't be shown in the text editor (an engine contract violation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelError(pub String);

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone)]
struct Snapshot {
    text: String,
    chunks: Vec<ChunkState>,
}

/// One version of the file plus its line index.
#[derive(Debug, Clone)]
struct Version {
    text: String,
    index: LineIndex,
}

impl Version {
    fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            index: LineIndex::new(text),
        }
    }

    fn slice(&self, lines: Range<usize>) -> &str {
        // Ranges are validated in `MergeModel::new`.
        self.index.slice(&self.text, lines).unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
struct SideRanges {
    ours: Range<usize>,
    theirs: Range<usize>,
    base: Range<usize>,
}

/// Undo history bounds.
const MAX_UNDO_STEPS: usize = 200;
const MAX_UNDO_BYTES: usize = 64 * 1024 * 1024;
/// Typing within this many seconds of the previous edit joins its undo step.
const COALESCE_SECS: f64 = 1.0;

/// The merge editor's document.
#[derive(Debug, Clone)]
pub struct MergeModel {
    ours: Version,
    theirs: Version,
    base: Version,
    sides: Vec<SideRanges>,
    template: String,
    template_chunks: Vec<ChunkState>,
    eol: &'static str,
    text: String,
    /// `text` as of the last sync (to diff hand edits against).
    shadow: String,
    out_index: LineIndex,
    chunks: Vec<ChunkState>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last_typing: Option<f64>,
    current: usize,
}

impl MergeModel {
    /// Validates the chunk ranges against the texts (never trusts them for slicing).
    pub fn new(conflict: &MergeConflict) -> Result<Self, ModelError> {
        let ours = Version::new(&conflict.ours);
        let theirs = Version::new(&conflict.theirs);
        let base = Version::new(&conflict.ancestor);
        let merged_index = LineIndex::new(&conflict.merged);
        let mut sides = Vec::with_capacity(conflict.chunks.len());
        let mut template_chunks = Vec::with_capacity(conflict.chunks.len());
        let mut prev_end = 0;
        for (i, c) in conflict.chunks.iter().enumerate() {
            let in_bounds = |r: &Range<usize>, idx: &LineIndex| r.start <= r.end && r.end <= idx.lines();
            let ok = c.is_well_formed()
                && in_bounds(&c.ours_range(), &ours.index)
                && in_bounds(&c.theirs_range(), &theirs.index)
                && in_bounds(&c.ancestor_range(), &base.index)
                && in_bounds(&c.merged_range(), &merged_index)
                && c.merged_start >= prev_end;
            if !ok {
                return Err(ModelError(format!("conflict {} has line ranges outside the file", i + 1)));
            }
            prev_end = c.merged_end;
            sides.push(SideRanges {
                ours: c.ours_range(),
                theirs: c.theirs_range(),
                base: c.ancestor_range(),
            });
            template_chunks.push(ChunkState {
                range: c.merged_range(),
                sel: ChunkSel::from_choice(Choice::BASE, c.ours_range().len(), c.theirs_range().len()),
                edited: false,
                kept: String::new(),
            });
        }
        let eol = dominant_eol(&[&conflict.ours, &conflict.theirs, &conflict.ancestor]);
        Ok(Self {
            ours,
            theirs,
            base,
            sides,
            template: conflict.merged.clone(),
            chunks: template_chunks.clone(),
            template_chunks,
            eol,
            text: conflict.merged.clone(),
            shadow: conflict.merged.clone(),
            out_index: merged_index,
            undo: Vec::new(),
            redo: Vec::new(),
            last_typing: None,
            current: 0,
        })
    }

    // ---- Reading ------------------------------------------------------------------------------------------

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn chunks(&self) -> &[ChunkState] {
        &self.chunks
    }

    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    #[cfg(test)]
    pub fn eol(&self) -> &'static str {
        self.eol
    }

    /// Line count of a pane's text.
    pub fn pane_lines(&self, pane: Pane) -> usize {
        match pane {
            Pane::Ours => self.ours.index.lines(),
            Pane::Theirs => self.theirs.index.lines(),
            Pane::Output => self.out_index.lines(),
        }
    }

    /// The whole text of a read-only side.
    pub fn side_text(&self, side: Side) -> &str {
        &self.version(side).text
    }

    /// Line `i` of a read-only side (with its terminator).
    pub fn side_line(&self, side: Side, i: usize) -> Option<&str> {
        let v = self.version(side);
        v.index.line(&v.text, i)
    }

    /// The side's text ends with a newline, so its pane shows an empty last row after it (as Monaco
    /// does; it is the final empty line of the old `split('\n')` layout).
    pub fn has_trailing_row(&self, side: Side) -> bool {
        self.version(side).text.ends_with('\n')
    }

    /// The chunk that the side's empty last row (see [`Self::has_trailing_row`]) belongs to: the one running to
    /// the end of the side, unless every version ends with a newline (the empty line is then common to all, as in
    /// the old layout's chunk boundaries).
    pub fn trailing_row_chunk(&self, side: Side) -> Option<usize> {
        if !self.has_trailing_row(side) || [&self.base, &self.ours, &self.theirs].iter().all(|v| v.text.ends_with('\n')) {
            return None;
        }
        let pane = match side {
            Side::Ours => Pane::Ours,
            Side::Theirs => Pane::Theirs,
        };
        let lines = self.pane_lines(pane);
        let last = self.chunks.len().checked_sub(1)?;
        (self.range_of(pane, last).end == lines).then_some(last)
    }

    /// Chunk `i`'s lines in a pane.
    pub fn chunk_range(&self, pane: Pane, i: usize) -> Option<Range<usize>> {
        match pane {
            Pane::Ours => self.sides.get(i).map(|s| s.ours.clone()),
            Pane::Theirs => self.sides.get(i).map(|s| s.theirs.clone()),
            Pane::Output => self.chunks.get(i).map(|c| c.range.clone()),
        }
    }

    /// The chunk covering `line` of a pane (binary search; chunks are sorted and disjoint).
    pub fn chunk_at(&self, pane: Pane, line: usize) -> Option<usize> {
        let i = self.first_chunk(pane, |r| r.end > line);
        (i < self.chunks.len() && self.range_of(pane, i).contains(&line)).then_some(i)
    }

    /// The first chunk whose lines start at `line` of a pane (it may be empty).
    pub fn chunk_starting_at(&self, pane: Pane, line: usize) -> Option<usize> {
        let i = self.first_chunk(pane, |r| r.start >= line);
        (i < self.chunks.len() && self.range_of(pane, i).start == line).then_some(i)
    }

    /// Binary search over the chunks for the first index where `pred(range)` holds (`pred` must be monotone;
    /// chunk ranges are sorted and disjoint in every pane).
    fn first_chunk(&self, pane: Pane, pred: impl Fn(&Range<usize>) -> bool) -> usize {
        let (mut lo, mut hi) = (0, self.chunks.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if pred(&self.range_of(pane, mid)) {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo
    }

    /// Chunks the user hasn't decided yet.
    pub fn undecided(&self) -> usize {
        self.chunks.iter().filter(|c| !c.is_decided()).count()
    }

    /// True if every chunk takes all of `side`.
    #[cfg(test)]
    pub fn all_take(&self, side: Side) -> bool {
        self.side_state(side) == Tri::All
    }

    /// The pane header's tri-state "Use all": every line of `side` in every chunk, some, or none.
    pub fn side_state(&self, side: Side) -> Tri {
        if self.chunks.is_empty() {
            return Tri::None;
        }
        let states: Vec<Tri> = self.chunks.iter().map(|c| c.sel.side(side).state()).collect();
        if states.iter().all(|s| *s == Tri::All) {
            Tri::All
        } else if states.iter().all(|s| *s == Tri::None) {
            Tri::None
        } else {
            Tri::Some
        }
    }

    pub fn has_markers(&self) -> bool {
        self.text.split_inclusive('\n').any(is_marker_line)
    }

    /// The output differs from the template.
    pub fn is_dirty(&self) -> bool {
        self.text != self.template
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn current(&self) -> usize {
        self.current
    }

    /// The text chunk `i` gets for `sel` below the kept hand edit `kept`: the taken lines in the order they were
    /// taken (a separator is added after a line without terminator when more lines follow); nothing taken and
    /// nothing kept = the base lines.
    pub fn content_for(&self, i: usize, sel: &ChunkSel, kept: &str) -> String {
        let Some(s) = self.sides.get(i) else { return String::new() };
        if !sel.is_taken() {
            if !kept.is_empty() {
                return kept.to_owned();
            }
            return self.base.slice(s.base.clone()).to_owned();
        }
        let lines = self.taken_lines(i, sel);
        let mut out = String::with_capacity(kept.len() + 2 + lines.iter().map(|(_, l)| l.len() + 2).sum::<usize>());
        out.push_str(kept);
        if !kept.is_empty() && !kept.ends_with('\n') {
            out.push_str(self.eol);
        }
        for (n, (_, line)) in lines.iter().enumerate() {
            out.push_str(line);
            if !line.ends_with('\n') && n + 1 < lines.len() {
                out.push_str(self.eol);
            }
        }
        out
    }

    /// The lines `sel` takes for chunk `i`, in output order, with the side each comes from.
    fn taken_lines(&self, i: usize, sel: &ChunkSel) -> Vec<(Side, &str)> {
        let Some(s) = self.sides.get(i) else { return Vec::new() };
        sel.order()
            .iter()
            .filter_map(|&(side, k)| {
                let range = match side {
                    Side::Ours => &s.ours,
                    Side::Theirs => &s.theirs,
                };
                let v = self.version(side);
                let text = (k < range.len()).then(|| v.index.line(&v.text, range.start + k)).flatten()?;
                Some((side, text))
            })
            .collect()
    }

    /// The side output line `k` of chunk `i` (relative to the chunk) was taken from; `None` while the chunk holds
    /// the base lines or was edited by hand, and for kept hand-edited lines.
    pub fn output_line_side(&self, i: usize, k: usize) -> Option<Side> {
        let c = self.chunks.get(i)?;
        if c.edited || !c.sel.is_taken() {
            return None;
        }
        let k = k.checked_sub(c.kept_lines())?;
        self.taken_lines(i, &c.sel).get(k).map(|(side, _)| *side)
    }

    // ---- Choices ------------------------------------------------------------------------------------------

    #[cfg(test)]
    /// Sets chunk `i` to a whole-side preset and re-renders that chunk (replacing hand edits inside it). One undo
    /// step.
    pub fn set_choice(&mut self, i: usize, choice: Choice) {
        let Some(s) = self.sides.get(i) else { return };
        let sel = ChunkSel::from_choice(choice, s.ours.len(), s.theirs.len());
        self.set_sel(i, sel);
    }

    fn set_sel(&mut self, i: usize, sel: ChunkSel) {
        if i >= self.chunks.len() {
            return;
        }
        self.checkpoint();
        self.apply_sel(i, sel);
        self.current = i;
    }

    /// The chunk spine of `side`: all lines taken → none; otherwise (none or some) → all, below the lines
    /// already taken from the other side.
    pub fn toggle(&mut self, i: usize, side: Side) {
        if let Some(c) = self.chunks.get(i) {
            let mut sel = c.sel.clone();
            let on = !sel.side(side).is_all();
            sel.set_side_all(side, on);
            self.set_sel(i, sel);
        }
    }

    /// The per-line pick chip: takes or drops line `k` (relative to the chunk) of `side`; a taken line goes below
    /// the lines already taken. One undo step.
    pub fn toggle_line(&mut self, i: usize, side: Side, k: usize) {
        if let Some(c) = self.chunks.get(i) {
            let mut sel = c.sel.clone();
            if sel.toggle_line(side, k) {
                self.set_sel(i, sel);
            }
        }
    }

    /// Takes (`on`) or drops every line of `side` in every chunk. One undo step.
    pub fn set_side_all(&mut self, side: Side, on: bool) {
        if self.chunks.is_empty() {
            return;
        }
        self.checkpoint();
        for i in 0..self.chunks.len() {
            let mut sel = self.chunks[i].sel.clone();
            sel.set_side_all(side, on);
            self.apply_sel(i, sel);
        }
    }

    /// Back to the template (undoable).
    pub fn reset(&mut self) {
        if !self.is_dirty() && self.chunks == self.template_chunks {
            return;
        }
        self.checkpoint();
        self.text = self.template.clone();
        self.chunks = self.template_chunks.clone();
        self.after_text_change();
    }

    /// Re-renders chunk `i` for `sel`. A hand edit in the chunk is kept, with the taken lines below it.
    fn apply_sel(&mut self, i: usize, sel: ChunkSel) {
        let Some(c) = self.chunks.get(i) else { return };
        let kept = if c.edited {
            self.out_index.slice(&self.text, c.range.clone()).unwrap_or_default().to_owned()
        } else {
            c.kept.clone()
        };
        let content = self.content_for(i, &sel, &kept);
        self.splice_chunk(i, content);
        if let Some(c) = self.chunks.get_mut(i) {
            c.sel = sel;
            c.edited = false;
            c.kept = kept;
        }
    }

    /// Replaces chunk `i`'s lines with `content`, keeping line structure (a separator is added where the
    /// content would otherwise run into a neighbouring line) and shifting the following chunks.
    fn splice_chunk(&mut self, i: usize, mut content: String) {
        let Some(range) = self.chunks.get(i).map(|c| c.range.clone()) else {
            return;
        };
        let Some(bytes) = self.out_index.byte_range(range.clone()) else {
            return;
        };
        let new_lines = count_lines(&content);
        if !content.is_empty() && bytes.end < self.text.len() && !content.ends_with('\n') {
            content.push_str(self.eol);
        }
        if !content.is_empty() && bytes.start > 0 && !self.text.get(..bytes.start).is_some_and(|t| t.ends_with('\n')) {
            content.insert_str(0, self.eol);
        }
        self.text.replace_range(bytes, &content);
        let new_end = range.start + new_lines;
        let delta = new_end as isize - range.end as isize;
        if let Some(c) = self.chunks.get_mut(i) {
            c.range = range.start..new_end;
        }
        for c in self.chunks.iter_mut().skip(i + 1) {
            c.range = shift(c.range.start, delta)..shift(c.range.end, delta);
        }
        self.after_text_change();
    }

    fn after_text_change(&mut self) {
        self.out_index = LineIndex::new(&self.text);
        self.shadow.clone_from(&self.text);
        self.current = self.current.min(self.chunks.len().saturating_sub(1));
    }

    // ---- Hand edits ---------------------------------------------------------------------------------------

    /// The buffer the output `TextEdit` edits. Call [`Self::sync_after_edit`] after every change.
    pub fn text_mut(&mut self) -> &mut String {
        &mut self.text
    }

    /// Re-derives chunk positions after the text was edited through [`Self::text_mut`]. `now` (seconds) groups
    /// quick consecutive typing into one undo step. Returns `false` if nothing changed.
    pub fn sync_after_edit(&mut self, now: f64) -> bool {
        if self.text == self.shadow {
            return false;
        }
        let coalesce = self.last_typing.is_some_and(|t| (now - t).abs() < COALESCE_SECS);
        if coalesce {
            self.redo.clear();
        } else {
            let snapshot = Snapshot {
                text: self.shadow.clone(),
                chunks: self.chunks.clone(),
            };
            self.push_undo(snapshot);
        }
        self.last_typing = Some(now);
        let old_index = std::mem::take(&mut self.out_index);
        let new_index = LineIndex::new(&self.text);
        let (a, old_end, new_end) = changed_lines(&self.shadow, &old_index, &self.text, &new_index);
        adjust_chunks(&mut self.chunks, a, old_end, new_end);
        self.out_index = new_index;
        self.shadow.clone_from(&self.text);
        true
    }

    // ---- Undo ---------------------------------------------------------------------------------------------

    /// Starts a new undo step (before a pick / reset).
    fn checkpoint(&mut self) {
        let snapshot = Snapshot {
            text: self.text.clone(),
            chunks: self.chunks.clone(),
        };
        self.push_undo(snapshot);
        self.last_typing = None;
    }

    fn push_undo(&mut self, snapshot: Snapshot) {
        self.undo.push(snapshot);
        self.redo.clear();
        let mut bytes: usize = self.undo.iter().map(|s| s.text.len()).sum();
        while self.undo.len() > MAX_UNDO_STEPS || (bytes > MAX_UNDO_BYTES && self.undo.len() > 1) {
            let dropped = self.undo.remove(0);
            bytes -= dropped.text.len();
        }
    }

    pub fn undo(&mut self) -> bool {
        let Some(snapshot) = self.undo.pop() else { return false };
        let current = self.restore(snapshot);
        self.redo.push(current);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(snapshot) = self.redo.pop() else { return false };
        let current = self.restore(snapshot);
        self.undo.push(current);
        true
    }

    fn restore(&mut self, snapshot: Snapshot) -> Snapshot {
        let current = Snapshot {
            text: std::mem::replace(&mut self.text, snapshot.text),
            chunks: std::mem::replace(&mut self.chunks, snapshot.chunks),
        };
        self.last_typing = None;
        self.after_text_change();
        current
    }

    // ---- Navigation ---------------------------------------------------------------------------------------

    pub fn set_current(&mut self, i: usize) {
        self.current = i.min(self.chunks.len().saturating_sub(1));
    }

    pub fn next(&mut self) -> bool {
        if self.current + 1 < self.chunks.len() {
            self.current += 1;
            true
        } else {
            false
        }
    }

    pub fn prev(&mut self) -> bool {
        if self.current > 0 && !self.chunks.is_empty() {
            self.current -= 1;
            true
        } else {
            false
        }
    }

    fn range_of(&self, pane: Pane, i: usize) -> Range<usize> {
        self.chunk_range(pane, i).unwrap_or_default()
    }

    fn version(&self, side: Side) -> &Version {
        match side {
            Side::Ours => &self.ours,
            Side::Theirs => &self.theirs,
        }
    }
}

fn shift(n: usize, delta: isize) -> usize {
    n.saturating_add_signed(delta)
}

/// Line-level difference between two buffers: old lines `a..old_end` became new lines `a..new_end` (common
/// prefix and suffix lines excluded).
pub fn changed_lines(old: &str, oi: &LineIndex, new: &str, ni: &LineIndex) -> (usize, usize, usize) {
    let (on, nn) = (oi.lines(), ni.lines());
    let mut p = 0;
    while p < on && p < nn && oi.line(old, p) == ni.line(new, p) {
        p += 1;
    }
    let mut s = 0;
    while s < on - p && s < nn - p && oi.line(old, on - 1 - s) == ni.line(new, nn - 1 - s) {
        s += 1;
    }
    (p, on - s, nn - s)
}

/// Updates chunk ranges after old lines `a..b` were replaced by new lines `a..c`. A chunk the edit touches is
/// marked edited, its side picks are dropped (the text is now the user's, so no line is "taken" any more; picking a
/// line or side again adds the picks below the edit) and it grows / shrinks to cover the edit; an edit spanning
/// several chunks merges them into the first (the others become empty at its end). Insertions exactly at a
/// non-empty chunk's start or end stay outside it; an insertion at an empty chunk's position fills that chunk.
pub fn adjust_chunks(chunks: &mut [ChunkState], a: usize, b: usize, c: usize) {
    let delta = c as isize - b as isize;
    let touched: Vec<usize> = (0..chunks.len())
        .filter(|&i| relation(&chunks[i].range, a, b) == Relation::Touched)
        .collect();
    for ch in chunks.iter_mut() {
        if relation(&ch.range, a, b) == Relation::After {
            ch.range = shift(ch.range.start, delta)..shift(ch.range.end, delta);
        }
    }
    if let (Some(&first), Some(&last)) = (touched.first(), touched.last()) {
        let start = chunks[first].range.start.min(a);
        let end = shift(chunks[last].range.end.max(b), delta);
        for (n, &i) in touched.iter().enumerate() {
            chunks[i].range = if n == 0 { start..end } else { end..end };
            chunks[i].edited = true;
            chunks[i].sel.clear();
            chunks[i].kept.clear();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relation {
    Before,
    Touched,
    After,
}

/// Where chunk lines `r` sit relative to an edit replacing old lines `a..b` (`a == b`: pure insertion at `a`).
fn relation(r: &Range<usize>, a: usize, b: usize) -> Relation {
    let (s, e) = (r.start, r.end);
    let touched = if s == e {
        (a == b && s == a) || (a < s && s < b)
    } else if a == b {
        s < a && a < e
    } else {
        s < b && a < e
    };
    if touched {
        Relation::Touched
    } else if (s == e && (s < a || (s == a && a < b))) || (s != e && e <= a) {
        Relation::Before
    } else {
        Relation::After
    }
}

#[cfg(test)]
mod tests {
    use gityas_core::models::{ConflictChunk, ConflictSides, ConflictType};

    use super::*;

    type R = (usize, usize);

    /// `chunks`: (ours, theirs, base, merged) line ranges.
    fn conflict(base: &str, ours: &str, theirs: &str, merged: &str, chunks: &[(R, R, R, R)]) -> MergeConflict {
        MergeConflict {
            ancestor: base.into(),
            ours: ours.into(),
            theirs: theirs.into(),
            merged: merged.into(),
            chunks: chunks
                .iter()
                .map(|&(o, t, b, m)| ConflictChunk {
                    conflict_type: ConflictType::Both,
                    ours_start: o.0,
                    ours_end: o.1,
                    theirs_start: t.0,
                    theirs_end: t.1,
                    ancestor_start: b.0,
                    ancestor_end: b.1,
                    merged_start: m.0,
                    merged_end: m.1,
                })
                .collect(),
            sides: ConflictSides::default(),
            ours_deleted: false,
            theirs_deleted: false,
            auto_resolved: chunks.is_empty(),
        }
    }

    fn model(c: &MergeConflict) -> MergeModel {
        MergeModel::new(c).unwrap_or_else(|e| panic!("{e}"))
    }

    /// One chunk in the middle: line 1.
    fn middle() -> MergeModel {
        model(&conflict(
            "a\nb\nc\n",
            "a\nO1\nO2\nc\n",
            "a\nT\nc\n",
            "a\nb\nc\n",
            &[((1, 3), (1, 2), (1, 2), (1, 2))],
        ))
    }

    /// Two chunks (lines 1 and 3 of the base), one auto-merged change in between (line 2 → "m").
    fn two() -> MergeModel {
        model(&conflict(
            "0\n1\n2\n3\n4\n",
            "0\no1\n2\no3a\no3b\n4\n",
            "0\nt1a\nt1b\nm\nt3\n4\n",
            "0\n1\nm\n3\n4\n",
            &[((1, 2), (1, 3), (1, 2), (1, 2)), ((3, 5), (4, 5), (3, 4), (3, 4))],
        ))
    }

    #[test]
    fn line_index() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("a"), 1);
        assert_eq!(count_lines("a\n"), 1);
        assert_eq!(count_lines("a\nb"), 2);
        assert_eq!(count_lines("a\r\nb\r\n"), 2);
        let idx = LineIndex::new("a\nbb\nc");
        assert_eq!(idx.slice("a\nbb\nc", 1..3), Some("bb\nc"));
        assert_eq!(idx.line("a\nbb\nc", 2), Some("c"));
        assert_eq!(idx.byte_range(2..4), None);
        assert_eq!(idx.byte_range(Range { start: 2, end: 1 }), None);
    }

    #[test]
    fn choose_ours_theirs_both_and_base() {
        let mut m = middle();
        assert_eq!(m.text(), "a\nb\nc\n");
        assert_eq!(m.undecided(), 1);
        m.set_choice(0, Choice::OURS);
        assert_eq!(m.text(), "a\nO1\nO2\nc\n");
        assert_eq!(m.chunks()[0].range, 1..3);
        m.set_choice(0, Choice::THEIRS);
        assert_eq!(m.text(), "a\nT\nc\n");
        m.set_choice(0, Choice::BOTH);
        assert_eq!(m.text(), "a\nO1\nO2\nT\nc\n");
        assert_eq!(m.chunks()[0].range, 1..4);
        assert_eq!(m.undecided(), 0);
        m.set_choice(0, Choice::BASE);
        assert_eq!(m.text(), "a\nb\nc\n");
        assert_eq!(m.undecided(), 1);
        assert!(!m.is_dirty());
    }

    #[test]
    fn toggling_sides_appends_them_in_click_order() {
        let mut m = middle();
        m.toggle(0, Side::Theirs);
        m.toggle(0, Side::Ours);
        assert_eq!(m.chunks()[0].sel.preset(), Some(Choice::BOTH));
        assert_eq!(m.text(), "a\nT\nO1\nO2\nc\n");
        m.toggle(0, Side::Ours);
        assert_eq!(m.text(), "a\nT\nc\n");
    }

    #[test]
    fn mixed_choices_shift_later_chunks_and_keep_auto_merged_lines() {
        let mut m = two();
        m.set_choice(0, Choice::THEIRS);
        assert_eq!(m.text(), "0\nt1a\nt1b\nm\n3\n4\n");
        assert_eq!(m.chunks()[1].range, 4..5);
        m.set_choice(1, Choice::OURS);
        assert_eq!(m.text(), "0\nt1a\nt1b\nm\no3a\no3b\n4\n");
        m.set_choice(0, Choice::BASE);
        assert_eq!(m.text(), "0\n1\nm\no3a\no3b\n4\n");
        assert_eq!(m.chunks()[1].range, 3..5);
        m.set_choice(0, Choice::BOTH);
        m.set_choice(1, Choice::BOTH);
        assert_eq!(m.text(), "0\no1\nt1a\nt1b\nm\no3a\no3b\nt3\n4\n");
        assert_eq!(m.current(), 1);
    }

    #[test]
    fn whole_side_checkbox() {
        let mut m = two();
        assert!(!m.all_take(Side::Ours));
        m.set_side_all(Side::Ours, true);
        assert!(m.all_take(Side::Ours));
        assert_eq!(m.text(), "0\no1\nm\no3a\no3b\n4\n");
        m.set_side_all(Side::Theirs, true);
        assert_eq!(m.text(), "0\no1\nt1a\nt1b\nm\no3a\no3b\nt3\n4\n");
        m.set_side_all(Side::Ours, false);
        assert_eq!(m.text(), "0\nt1a\nt1b\nm\nt3\n4\n");
        // One undo step per whole-side action.
        assert!(m.undo());
        assert_eq!(m.text(), "0\no1\nt1a\nt1b\nm\no3a\no3b\nt3\n4\n");
    }

    #[test]
    fn reset_undo_redo() {
        let mut m = two();
        m.set_choice(0, Choice::OURS);
        m.set_choice(1, Choice::THEIRS);
        let chosen = m.text().to_owned();
        m.reset();
        assert_eq!(m.text(), "0\n1\nm\n3\n4\n");
        assert_eq!(m.undecided(), 2);
        assert!(m.undo());
        assert_eq!(m.text(), chosen);
        assert!(m.undo());
        assert_eq!(m.text(), "0\no1\nm\n3\n4\n");
        assert_eq!(m.chunks()[1].sel.preset(), Some(Choice::BASE));
        assert!(m.redo() && m.redo());
        assert_eq!(m.text(), "0\n1\nm\n3\n4\n");
        assert!(!m.redo());
        // A new action clears redo.
        assert!(m.undo());
        m.set_choice(0, Choice::THEIRS);
        assert!(!m.can_redo());
    }

    #[test]
    fn chunks_at_start_and_end_of_file() {
        let c = conflict(
            "s\nx\ne\n",
            "S1\nx\nE1\n",
            "S2\nx\nE2\n",
            "s\nx\ne\n",
            &[((0, 1), (0, 1), (0, 1), (0, 1)), ((2, 3), (2, 3), (2, 3), (2, 3))],
        );
        let mut m = model(&c);
        m.set_choice(0, Choice::BOTH);
        m.set_choice(1, Choice::BOTH);
        assert_eq!(m.text(), "S1\nS2\nx\nE1\nE2\n");
        m.set_choice(0, Choice::THEIRS);
        m.set_choice(1, Choice::OURS);
        assert_eq!(m.text(), "S2\nx\nE1\n");
    }

    #[test]
    fn no_trailing_newline_is_kept_and_both_gets_a_separator() {
        let c = conflict("a\nb", "a\nX", "a\nY", "a\nb", &[((1, 2), (1, 2), (1, 2), (1, 2))]);
        let mut m = model(&c);
        m.set_choice(0, Choice::OURS);
        assert_eq!(m.text(), "a\nX");
        m.set_choice(0, Choice::BOTH);
        assert_eq!(m.text(), "a\nX\nY");
        m.set_choice(0, Choice::THEIRS);
        assert_eq!(m.text(), "a\nY");
        // A side without a final newline in front of more text gets one.
        let c = conflict("a\nb\n", "a\nX", "a\nY\n", "a\nb\n", &[((1, 2), (1, 2), (1, 2), (1, 2))]);
        let mut m = model(&c);
        m.set_choice(0, Choice::BOTH);
        assert_eq!(m.text(), "a\nX\nY\n");
    }

    #[test]
    fn crlf_is_preserved_and_used_for_separators() {
        let c = conflict(
            "a\r\nb\r\nz\r\n",
            "a\r\nX",
            "a\r\nY\r\nz\r\n",
            "a\r\nb\r\nz\r\n",
            &[((1, 2), (1, 3), (1, 3), (1, 3))],
        );
        let mut m = model(&c);
        assert_eq!(m.eol(), "\r\n");
        m.set_choice(0, Choice::BOTH);
        assert_eq!(m.text(), "a\r\nX\r\nY\r\nz\r\n");
        m.set_choice(0, Choice::OURS);
        assert_eq!(m.text(), "a\r\nX");
        assert_eq!(dominant_eol(&["a\nb\n", "c\r\n"]), "\n");
    }

    #[test]
    fn empty_sides() {
        // Ours deleted the conflicting lines, theirs changed them.
        let c = conflict("a\nb\nc\n", "a\nc\n", "a\nB\nc\n", "a\nb\nc\n", &[((1, 1), (1, 2), (1, 2), (1, 2))]);
        let mut m = model(&c);
        m.set_choice(0, Choice::OURS);
        assert_eq!(m.text(), "a\nc\n");
        assert_eq!(m.chunks()[0].range, 1..1);
        m.set_choice(0, Choice::BOTH);
        assert_eq!(m.text(), "a\nB\nc\n");
        // Add/add: no base, empty template.
        let c = conflict("", "x\n", "y\n", "", &[((0, 1), (0, 1), (0, 0), (0, 0))]);
        let mut m = model(&c);
        assert_eq!(m.text(), "");
        m.set_choice(0, Choice::BOTH);
        assert_eq!(m.text(), "x\ny\n");
        m.set_choice(0, Choice::BASE);
        assert_eq!(m.text(), "");
        // Both sides empty in the chunk (both deleted different extra lines around the same spot).
        let c = conflict("a\n", "", "", "a\n", &[((0, 0), (0, 0), (0, 1), (0, 1))]);
        let mut m = model(&c);
        m.set_choice(0, Choice::BOTH);
        assert_eq!(m.text(), "");
    }

    #[test]
    fn auto_resolved_has_no_chunks() {
        let c = conflict("a\n", "b\n", "a\n", "b\n", &[]);
        let mut m = model(&c);
        assert_eq!((m.chunk_count(), m.undecided(), m.text()), (0, 0, "b\n"));
        assert!(!m.next() && !m.prev());
        m.set_side_all(Side::Ours, true);
        m.set_choice(3, Choice::OURS);
        assert!(!m.can_undo() && !m.all_take(Side::Ours));
    }

    #[test]
    fn invalid_ranges_are_rejected() {
        let bad = conflict("a\n", "a\n", "a\n", "a\n", &[((0, 5), (0, 1), (0, 1), (0, 1))]);
        assert!(MergeModel::new(&bad).is_err());
        let overlapping = conflict(
            "a\nb\n",
            "a\nb\n",
            "a\nb\n",
            "a\nb\n",
            &[((0, 1), (0, 1), (0, 1), (0, 2)), ((1, 2), (1, 2), (1, 2), (1, 2))],
        );
        assert!(MergeModel::new(&overlapping).is_err());
        let inverted = conflict("a\n", "a\n", "a\n", "a\n", &[((1, 0), (0, 1), (0, 1), (0, 1))]);
        assert!(MergeModel::new(&inverted).is_err());
    }

    fn edit(m: &mut MergeModel, text: &str, now: f64) {
        *m.text_mut() = text.to_owned();
        assert!(m.sync_after_edit(now));
    }

    #[test]
    fn edits_outside_a_chunk_survive_a_choice() {
        let mut m = two();
        // Change line 0, then insert a line before chunk 1 (each frame sees one local edit).
        edit(&mut m, "ZERO\n1\nm\n3\n4\n", 0.0);
        edit(&mut m, "ZERO\n1\nm\nnew\n3\n4\n", 0.1);
        assert_eq!(m.chunks()[0].range, 1..2);
        assert_eq!(m.chunks()[1].range, 4..5);
        assert!(!m.chunks()[0].edited && !m.chunks()[1].edited);
        m.set_choice(1, Choice::OURS);
        m.set_choice(0, Choice::THEIRS);
        assert_eq!(m.text(), "ZERO\nt1a\nt1b\nm\nnew\no3a\no3b\n4\n");
    }

    #[test]
    fn edits_inside_a_chunk_mark_it_edited_and_a_choice_goes_below_them() {
        let mut m = two();
        edit(&mut m, "0\none\nextra\nm\n3\n4\n", 0.0);
        assert_eq!(m.chunks()[0].range, 1..3);
        assert!(m.chunks()[0].edited);
        assert_eq!(m.chunks()[1].range, 4..5);
        assert_eq!(m.undecided(), 1);
        m.set_choice(0, Choice::OURS);
        assert_eq!(m.text(), "0\none\nextra\no1\nm\n3\n4\n");
        assert_eq!(m.chunks()[0].range, 1..4);
        assert!(!m.chunks()[0].edited && m.chunks()[0].is_decided());
        // Kept lines have no side; dropping the pick leaves the edit, not the base lines.
        let sides: Vec<_> = (0..3).map(|k| m.output_line_side(0, k)).collect();
        assert_eq!(sides, [None, None, Some(Side::Ours)]);
        m.set_choice(0, Choice::BASE);
        assert_eq!(m.text(), "0\none\nextra\nm\n3\n4\n");
        assert_eq!(m.undecided(), 1);
        assert!(m.undo());
        assert!(m.undo());
        assert_eq!(m.text(), "0\none\nextra\nm\n3\n4\n");
        assert!(m.chunks()[0].edited);
    }

    #[test]
    fn deleting_a_whole_chunk_and_typing_into_an_empty_chunk() {
        let mut m = two();
        edit(&mut m, "0\nm\n3\n4\n", 0.0);
        assert_eq!(m.chunks()[0].range, 1..1);
        assert!(m.chunks()[0].edited);
        assert_eq!(m.chunks()[1].range, 2..3);
        edit(&mut m, "0\nback\nm\n3\n4\n", 5.0);
        assert_eq!(m.chunks()[0].range, 1..2);
        assert_eq!(m.chunks()[1].range, 3..4);
    }

    #[test]
    fn insertions_at_chunk_edges_stay_outside() {
        let mut m = middle();
        edit(&mut m, "a\nbefore\nb\nc\n", 0.0);
        assert_eq!(m.chunks()[0].range, 2..3);
        edit(&mut m, "a\nbefore\nb\nafter\nc\n", 5.0);
        assert_eq!(m.chunks()[0].range, 2..3);
        assert!(!m.chunks()[0].edited);
    }

    #[test]
    fn an_edit_spanning_two_chunks_merges_them_into_the_first() {
        let mut m = two();
        edit(&mut m, "0\nX\n4\n", 0.0);
        assert_eq!(m.chunks()[0].range, 1..2);
        assert_eq!(m.chunks()[1].range, 2..2);
        assert!(m.chunks().iter().all(|c| c.edited));
        // Choosing a side for the second (now empty) chunk inserts there.
        m.set_choice(1, Choice::THEIRS);
        assert_eq!(m.text(), "0\nX\nt3\n4\n");
    }

    #[test]
    fn quick_typing_is_one_undo_step() {
        let mut m = middle();
        edit(&mut m, "ab\nb\nc\n", 0.0);
        edit(&mut m, "abc\nb\nc\n", 0.3);
        edit(&mut m, "abcd\nb\nc\n", 0.6);
        edit(&mut m, "abcde\nb\nc\n", 3.0);
        assert!(m.undo());
        assert_eq!(m.text(), "abcd\nb\nc\n");
        assert!(m.undo());
        assert_eq!(m.text(), "a\nb\nc\n");
        assert!(!m.undo());
        assert!(!m.sync_after_edit(9.0));
    }

    #[test]
    fn undo_history_is_bounded() {
        let mut m = middle();
        for i in 0..(MAX_UNDO_STEPS + 20) {
            m.set_choice(0, if i % 2 == 0 { Choice::OURS } else { Choice::THEIRS });
        }
        let mut n = 0;
        while m.undo() {
            n += 1;
        }
        assert_eq!(n, MAX_UNDO_STEPS);
    }

    #[test]
    fn navigation_clamps() {
        let mut m = two();
        assert_eq!(m.current(), 0);
        assert!(!m.prev());
        assert!(m.next());
        assert!(!m.next());
        assert_eq!(m.current(), 1);
        m.set_current(7);
        assert_eq!(m.current(), 1);
    }

    #[test]
    fn chunk_lookup() {
        let m = two();
        assert_eq!(m.chunk_at(Pane::Theirs, 2), Some(0));
        assert_eq!(m.chunk_at(Pane::Theirs, 3), None);
        assert_eq!(m.chunk_at(Pane::Ours, 4), Some(1));
        assert_eq!(m.chunk_at(Pane::Output, 3), Some(1));
        assert_eq!(m.chunk_at(Pane::Output, 9), None);
        assert_eq!(m.chunk_starting_at(Pane::Theirs, 4), Some(1));
        assert_eq!(m.chunk_starting_at(Pane::Theirs, 2), None);
        let c = conflict("a\nb\nc\n", "a\nc\n", "a\nB\nc\n", "a\nb\nc\n", &[((1, 1), (1, 2), (1, 2), (1, 2))]);
        let empty = model(&c);
        assert_eq!(empty.chunk_starting_at(Pane::Ours, 1), Some(0));
        assert_eq!(empty.chunk_at(Pane::Ours, 1), None);
    }

    #[test]
    fn conflict_markers_are_detected() {
        assert!(is_marker_line("<<<<<<< HEAD\n"));
        assert!(is_marker_line(">>>>>>> feature\r\n"));
        assert!(is_marker_line("<<<<<<<"));
        assert!(!is_marker_line("<<<<<<<< eight"));
        assert!(!is_marker_line("=======\n"));
        assert!(!is_marker_line("  <<<<<<< indented"));
        let mut m = middle();
        assert!(!m.has_markers());
        edit(&mut m, "a\n<<<<<<< ours\nb\nc\n", 0.0);
        assert!(m.has_markers());
    }

    // ---- Per-line choices ------------------------------------------------------------------------------

    #[test]
    fn per_line_choices_go_below_the_lines_already_taken() {
        let mut m = middle();
        // Click order theirs, then the second ours line: the output follows the click order.
        m.toggle_line(0, Side::Theirs, 0);
        m.toggle_line(0, Side::Ours, 1);
        assert_eq!(m.text(), "a\nT\nO2\nc\n");
        assert_eq!(m.chunks()[0].sel.ours.state(), Tri::Some);
        assert_eq!(m.chunks()[0].sel.theirs.state(), Tri::All);
        assert_eq!(m.chunks()[0].sel.preset(), None);
        assert_eq!(m.side_state(Side::Ours), Tri::Some);
        assert!(m.chunks()[0].is_decided() && m.undecided() == 0);
        // Dropping every line goes back to the base lines.
        m.toggle_line(0, Side::Ours, 1);
        m.toggle_line(0, Side::Theirs, 0);
        assert_eq!(m.text(), "a\nb\nc\n");
        assert_eq!(m.undecided(), 1);
        // Out-of-range lines are ignored (no undo step).
        let steps = m.undo.len();
        m.toggle_line(0, Side::Ours, 9);
        m.toggle_line(7, Side::Ours, 0);
        assert_eq!(m.undo.len(), steps);
    }

    #[test]
    fn chunk_checkbox_from_partial_takes_all_then_none() {
        let mut m = middle();
        m.toggle_line(0, Side::Ours, 0);
        assert_eq!(m.text(), "a\nO1\nc\n");
        m.toggle(0, Side::Ours);
        assert_eq!(m.text(), "a\nO1\nO2\nc\n");
        assert_eq!(m.chunks()[0].sel.preset(), Some(Choice::OURS));
        m.toggle(0, Side::Ours);
        assert_eq!(m.text(), "a\nb\nc\n");
        // Whole side from a partial chunk: the whole side moves below the lines already taken.
        m.toggle_line(0, Side::Theirs, 0);
        m.toggle_line(0, Side::Ours, 1);
        m.set_side_all(Side::Ours, true);
        assert_eq!(m.text(), "a\nT\nO1\nO2\nc\n");
        // Re-taking a dropped line puts it at the bottom.
        m.toggle_line(0, Side::Ours, 0);
        m.toggle_line(0, Side::Ours, 0);
        assert_eq!(m.text(), "a\nT\nO2\nO1\nc\n");
        assert_eq!(m.side_state(Side::Ours), Tri::All);
        // Each output line knows the side it came from.
        let sides: Vec<_> = (0..4).map(|k| m.output_line_side(0, k)).collect();
        assert_eq!(sides, [Some(Side::Theirs), Some(Side::Ours), Some(Side::Ours), None]);
        // Base lines and hand-edited chunks have no side.
        m.toggle(0, Side::Ours);
        m.toggle(0, Side::Theirs);
        assert_eq!(m.text(), "a\nb\nc\n");
        assert_eq!(m.output_line_side(0, 0), None);
        m.toggle_line(0, Side::Ours, 0);
        m.text_mut().replace_range(2..4, "X1");
        assert!(m.sync_after_edit(0.0));
        assert_eq!(m.output_line_side(0, 0), None);
    }

    #[test]
    fn per_line_choice_is_undoable_and_goes_below_hand_edits() {
        let mut m = middle();
        m.toggle_line(0, Side::Ours, 1);
        m.text_mut().replace_range(2..5, "X2\n");
        assert!(m.sync_after_edit(0.0));
        assert!(m.chunks()[0].edited);
        // The edit dropped the pick, so picking again starts from nothing, below the edit.
        m.toggle_line(0, Side::Ours, 0);
        assert_eq!(m.text(), "a\nX2\nO1\nc\n");
        assert!(!m.chunks()[0].edited);
        m.toggle_line(0, Side::Theirs, 0);
        assert_eq!(m.text(), "a\nX2\nO1\nT\nc\n");
        assert!(m.undo());
        assert!(m.undo());
        assert_eq!(m.text(), "a\nX2\nc\n");
        assert!(m.undo());
        assert_eq!(m.text(), "a\nO2\nc\n");
        assert!(m.undo());
        assert_eq!(m.text(), "a\nb\nc\n");
    }

    #[test]
    fn editing_a_mixed_chunk_unchecks_both_sides_until_picked_again() {
        let mut m = middle();
        m.toggle_line(0, Side::Ours, 0);
        m.toggle_line(0, Side::Theirs, 0);
        assert_eq!(m.text(), "a\nO1\nT\nc\n");
        edit(&mut m, "a\nO1 edited\nT\nc\n", 0.0);
        let c = &m.chunks()[0];
        assert!(c.edited && c.is_decided());
        assert_eq!((c.sel.ours.state(), c.sel.theirs.state()), (Tri::None, Tri::None));
        assert!(c.sel.order().is_empty());
        assert_eq!(m.side_state(Side::Ours), Tri::None);
        assert_eq!(m.text(), "a\nO1 edited\nT\nc\n", "the edit itself is kept");
        // Either side can be added back, below the edit.
        m.toggle(0, Side::Theirs);
        assert_eq!(m.text(), "a\nO1 edited\nT\nT\nc\n");
        assert_eq!(m.chunks()[0].sel.theirs.state(), Tri::All);
        // Undo brings back the edit and its cleared picks, then the picks from before the edit.
        assert!(m.undo());
        assert_eq!(m.chunks()[0].sel.theirs.state(), Tri::None);
        assert!(m.undo());
        assert_eq!(m.chunks()[0].sel.ours.state(), Tri::Some);
        assert_eq!(m.chunks()[0].sel.theirs.state(), Tri::All);
    }

    #[test]
    fn per_line_choice_adds_a_separator_after_a_last_line_without_newline() {
        // The chunk is the last line of each side, without a trailing newline.
        let mut m = model(&conflict("a\nb", "a\nO", "a\nT", "a\nb", &[((1, 2), (1, 2), (1, 2), (1, 2))]));
        m.toggle_line(0, Side::Ours, 0);
        assert_eq!(m.text(), "a\nO");
        m.toggle_line(0, Side::Theirs, 0);
        assert_eq!(
            m.text(),
            "a\nO\nT",
            "separator between the two lines, still no newline at end of file"
        );
    }

    #[test]
    fn trailing_row_belongs_to_a_chunk_at_the_end_unless_every_version_ends_with_a_newline() {
        // `mod_rs_edge_case` shape: ours adds lines with a final newline; base and theirs have none.
        let m = model(&conflict("b", "O1\nO2\n", "T", "b", &[((0, 2), (0, 1), (0, 1), (0, 1))]));
        assert!(m.has_trailing_row(Side::Ours) && !m.has_trailing_row(Side::Theirs));
        assert_eq!(m.trailing_row_chunk(Side::Ours), Some(0));
        assert_eq!(m.trailing_row_chunk(Side::Theirs), None);
        // Every version ends with a newline: the empty line is common, not part of the chunk.
        let m = model(&conflict("b\n", "O1\nO2\n", "T\n", "b\n", &[((0, 2), (0, 1), (0, 1), (0, 1))]));
        assert_eq!(m.trailing_row_chunk(Side::Ours), None);
        // A chunk that does not reach the end.
        let m = model(&conflict("b\nc", "O\nc\n", "T\nc", "b\nc", &[((0, 1), (0, 1), (0, 1), (0, 1))]));
        assert_eq!(m.trailing_row_chunk(Side::Ours), None);
    }

    #[test]
    fn an_empty_side_can_be_taken_as_a_whole() {
        // Ours deleted line 1; theirs changed it.
        let mut m = model(&conflict(
            "a\nb\nc\n",
            "a\nc\n",
            "a\nT\nc\n",
            "a\nb\nc\n",
            &[((1, 1), (1, 2), (1, 2), (1, 2))],
        ));
        assert_eq!(m.chunks()[0].sel.ours.state(), Tri::None);
        m.toggle(0, Side::Ours);
        assert_eq!(m.text(), "a\nc\n", "taking the empty side deletes the base line");
        assert_eq!(m.chunks()[0].sel.ours.state(), Tri::All);
        assert!(m.chunks()[0].is_decided());
        // No per-line toggles exist on an empty side.
        let steps = m.undo.len();
        m.toggle_line(0, Side::Ours, 0);
        assert_eq!(m.undo.len(), steps);
        m.toggle_line(0, Side::Theirs, 0);
        assert_eq!(m.text(), "a\nT\nc\n");
    }
}
