//! Merge-editor chunk layout: where each conflict sits in ours, theirs, the base and the editor's template, laid
//! out the way the Tauri app showed them.
//!
//! An O(NP) diff + diff3 merge: the old app's chunker
//! (`misc/gityas_myers.rs`, `misc/gityas_merger.rs`, `git_rs/files.rs::get_conflicts_by_files`) approximated it and
//! was restored after the migration replaced it with [`super::engine`] (git's `xdl_do_merge`); WP6.18 swapped its
//! Myers diff for an O(NP) one. Git's and this one are both valid merges, but they align conflicts differently:
//! xdiff's heuristics can pair a side's changed lines with a far-away, similar block (`complex_5_chunks`: theirs at
//! line 204 instead of the matching `analyze_conflicts` lines at 54), and git's EAGER level folds neighbouring
//! conflicts together. This layout is:
//! * An O(NP) line diff for base→ours and base→theirs. Where several shortest
//!   diffs exist, which one it picks decides what merges cleanly, so its tie-breaking is ported exactly;
//! * diff3 grouping: one conflict per group of changes that overlap or touch in the base, holding the
//!   base lines, unless both sides made the same change.
//!
//! The old chunker's display trims (a common suffix within 20 lines left outside the chunk, repeated context after a
//! conflict not duplicated) compensated for its Myers diff and are gone: without them the layout matches the reference
//! layout on every fixture and on thousands of random merges (WP6.18).
//!
//! `tests/wp6_legacy_suite.rs` is the old `misc/tests.rs`, unchanged numbers included; `tests/wp6_reference_layout.rs`
//! adds cases checked against reference layouts.
//!
//! **Line model** (the old one, Monaco's): a line is a `split('\n')` piece, so a text ending in `\n` has a final
//! empty line, and every range is 0-based and end-exclusive. [`super::conflict::conflict_from_texts`] converts to
//! the editor's `split_inclusive('\n')` model, which only drops that final empty line.
//!
//! Fixed against the old code (none of these change the old fixtures):
//! * WP6.1: a delete+insert at the very end of the edit script emitted its insertion twice.
//! * WP6.2: a group of several hunks on one side and none on the other panicked (`coalesce_hunks` of nothing).
//! * WP6.4: `files.rs` turned the ranges into "1-based unless 0", which the old frontend read as 0-based; the two
//!   offsets cancelled except for a chunk starting at line 1, reported as 0. Ranges are now computed directly.
//! * WP6.6: the old Myers trace blew up on large rewrites. The diff gives up past [`MAX_EDIT_DISTANCE`] (or
//!   [`MAX_SNAKES`] recorded runs); the caller then uses git's layout.
//! * No indexing that can panic: inconsistent intermediate results give `None` (git's layout is used instead).

use std::collections::VecDeque;
use std::ops::Range;

/// Largest edit distance (inserted + deleted lines, per side) the diff runs to before giving up.
pub const MAX_EDIT_DISTANCE: usize = 2_500;

/// One conflict chunk. Ranges are 0-based, end-exclusive, in `split('\n')` lines (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorChunk {
    /// Lines of the base the chunk replaces; the template holds exactly these lines at [`Self::merged`].
    pub base: Range<usize>,
    pub ours: Range<usize>,
    pub theirs: Range<usize>,
    /// Position in [`EditorMerge::merged`].
    pub merged: Range<usize>,
}

/// Result of [`editor_chunks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorMerge {
    /// The template: every non-conflicting change applied, each chunk holding its base lines.
    pub merged: String,
    pub chunks: Vec<EditorChunk>,
}

/// Splits into the old (Monaco) line model.
pub fn split_pieces(text: &str) -> Vec<&str> {
    text.split('\n').collect()
}

/// Lays out the conflicts of a three-way merge for the merge editor. `None` if a side's edit distance exceeds
/// [`MAX_EDIT_DISTANCE`] or the layout is inconsistent; callers then fall back to [`super::engine::merge_texts`].
pub fn editor_chunks(base: &str, ours: &str, theirs: &str) -> Option<EditorMerge> {
    let base_lines = split_pieces(base);
    let ours_lines = split_pieces(ours);
    let theirs_lines = split_pieces(theirs);
    let ours_hunks = changes(&base_lines, &ours_lines)?;
    let theirs_hunks = changes(&base_lines, &theirs_lines)?;
    let groups = merge_diffs(ours_hunks, theirs_hunks, &ours_lines, &theirs_lines)?;
    let merge = apply_merged_chunks(&base_lines, &ours_lines, &theirs_lines, groups)?;
    is_consistent(&merge, &base_lines, &ours_lines, &theirs_lines).then_some(merge)
}

// ---- Line diff (O(NP)) -------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditType {
    Insert,
    Delete,
    Replace,
}

/// `old[old_start..old_end]` became `new[new_start..new_end]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hunk {
    old_start: usize,
    old_end: usize,
    new_start: usize,
    new_end: usize,
    kind: EditType,
}

/// The changed regions of base→side, in order.
fn changes(old: &[&str], new: &[&str]) -> Option<Vec<Hunk>> {
    Some(
        onp(old, new)?
            .into_iter()
            .map(|(o, n)| Hunk {
                kind: match (o.is_empty(), n.is_empty()) {
                    (true, _) => EditType::Insert,
                    (_, true) => EditType::Delete,
                    _ => EditType::Replace,
                },
                old_start: o.start,
                old_end: o.end,
                new_start: n.start,
                new_end: n.end,
            })
            .collect(),
    )
}

/// Most equal runs the diff records before giving up (~20 MB); see [`MAX_EDIT_DISTANCE`].
const MAX_SNAKES: usize = 1_000_000;

/// A run of equal lines on the diff path, `a[start_x..end_x] == b[start_y..end_y]`, after the run `prev`.
#[derive(Debug, Clone, Copy)]
struct Snake {
    start_x: u32,
    start_y: u32,
    end_x: u32,
    end_y: u32,
    prev: u32,
}

const NO_SNAKE: u32 = u32::MAX;

/// Line diff (Wu, Manber, Myers and Miller's O(NP) algorithm): the changed regions as `(old range, new range)`.
/// Several shortest diffs often exist, and which one is picked decides what merges cleanly and what conflicts
/// (`files_rs_insert_before_conflict`: a plain Myers diff pairs a lone `};` and puts ~110 inserted lines inside the
/// conflict, this diff merges them above it), so its tie-breaking is kept as is, including diffing new→old when
/// `old` is not the shorter text.
fn onp(old: &[&str], new: &[&str]) -> Option<Vec<(Range<usize>, Range<usize>)>> {
    let swap = old.len() >= new.len();
    let (a, b) = if swap { (new, old) } else { (old, new) };
    let (m, n) = (isize::try_from(a.len()).ok()?, isize::try_from(b.len()).ok()?);
    // Snakes store positions as `u32` (`n` is the longer text).
    u32::try_from(n).ok()?;
    let offset = m + 1;
    let size = usize::try_from(m + n + 3).ok()?;
    // Per diagonal `k = y - x`: the furthest `y` reached, and the last snake on that path.
    let mut fp = vec![-1isize; size];
    let mut path = vec![NO_SNAKE; size];
    let mut snakes: Vec<Snake> = Vec::new();
    let idx = |k: isize| usize::try_from(k + offset).ok();
    let mut snake = |fp: &mut [isize], path: &mut [u32], k: isize| -> Option<()> {
        let (below, above) = (*fp.get(idx(k - 1)?)? + 1, *fp.get(idx(k + 1)?)?);
        let prev = if below > above {
            *path.get(idx(k - 1)?)?
        } else {
            *path.get(idx(k + 1)?)?
        };
        let mut y = below.max(above);
        let mut x = y - k;
        let (start_x, start_y) = (x, y);
        while x < m && y < n && x >= 0 && y >= 0 && a.get(x as usize) == b.get(y as usize) {
            x += 1;
            y += 1;
        }
        *path.get_mut(idx(k)?)? = if (x, y) == (start_x, start_y) {
            prev
        } else {
            if snakes.len() >= MAX_SNAKES {
                return None;
            }
            snakes.push(Snake {
                start_x: u32::try_from(start_x).ok()?,
                start_y: u32::try_from(start_y).ok()?,
                end_x: u32::try_from(x).ok()?,
                end_y: u32::try_from(y).ok()?,
                prev,
            });
            u32::try_from(snakes.len() - 1).ok()?
        };
        *fp.get_mut(idx(k)?)? = y;
        Some(())
    };
    let delta = n - m;
    let mut p = -1isize;
    loop {
        p += 1;
        if usize::try_from(delta + 2 * p).ok()? > MAX_EDIT_DISTANCE {
            return None;
        }
        for k in -p..delta {
            snake(&mut fp, &mut path, k)?;
        }
        for k in (delta + 1..=delta + p).rev() {
            snake(&mut fp, &mut path, k)?;
        }
        snake(&mut fp, &mut path, delta)?;
        if *fp.get(idx(delta)?)? == n {
            break;
        }
    }
    // Walk the snakes back from the end; the gaps between them are the changes.
    let mut regions = Vec::new();
    let (mut last_x, mut last_y) = (a.len(), b.len());
    let mut r = *path.get(idx(delta)?)?;
    let mut gap = |x: usize, y: usize, last_x: usize, last_y: usize| {
        if (x, y) != (last_x, last_y) {
            regions.push(if swap { (y..last_y, x..last_x) } else { (x..last_x, y..last_y) });
        }
    };
    while r != NO_SNAKE {
        let s = *snakes.get(r as usize)?;
        gap(s.end_x as usize, s.end_y as usize, last_x, last_y);
        (last_x, last_y) = (s.start_x as usize, s.start_y as usize);
        r = s.prev;
    }
    gap(0, 0, last_x, last_y);
    regions.reverse();
    Some(regions)
}

// ---- Merge (diff3 grouping; the old `gityas_merger.rs` grouped the same way) --------------------------------

#[derive(Debug, Clone)]
enum MergedChunk {
    Ours(Hunk),
    Theirs(Hunk),
    /// Both sides made the same change.
    Both(Hunk),
    Conflict(Hunk, Hunk),
}

fn merge_diffs(ours: Vec<Hunk>, theirs: Vec<Hunk>, ours_lines: &[&str], theirs_lines: &[&str]) -> Option<Vec<MergedChunk>> {
    let (mut ours, mut theirs) = (VecDeque::from(ours), VecDeque::from(theirs));
    let mut result = Vec::new();
    while !ours.is_empty() || !theirs.is_empty() {
        // Seed with whichever side starts earliest (on a tie, ours only if it is a pure insertion).
        let seed_ours = match (ours.front(), theirs.front()) {
            (_, None) => true,
            (None, _) => false,
            (Some(o), Some(t)) if o.old_start != t.old_start => o.old_start < t.old_start,
            (Some(o), Some(_)) => o.kind == EditType::Insert,
        };
        let (mut got_ours, mut got_theirs): (Vec<Hunk>, Vec<Hunk>) = (Vec::new(), Vec::new());
        if seed_ours {
            got_ours.extend(ours.pop_front());
        } else {
            got_theirs.extend(theirs.pop_front());
        }
        // Absorb every hunk on either side that touches or overlaps the group, until nothing changes.
        loop {
            let before = got_ours.len() + got_theirs.len();
            let high = high_mark(&got_ours, &got_theirs);
            while let Some(h) = theirs.front().filter(|h| h.old_start <= high).copied() {
                theirs.pop_front();
                got_theirs.push(h);
            }
            let high = high_mark(&got_ours, &got_theirs);
            while let Some(h) = ours.front().filter(|h| h.old_start <= high).copied() {
                ours.pop_front();
                got_ours.push(h);
            }
            if got_ours.len() + got_theirs.len() == before {
                break;
            }
        }
        match (got_ours.as_slice(), got_theirs.as_slice()) {
            ([h], []) => result.push(MergedChunk::Ours(*h)),
            ([], [h]) => result.push(MergedChunk::Theirs(*h)),
            // WP6.2: several touching hunks on one side only. They are contiguous in both files, so one hunk.
            (hs, []) => result.push(MergedChunk::Ours(coalesce(hs)?)),
            ([], hs) => result.push(MergedChunk::Theirs(coalesce(hs)?)),
            ([o], [t]) if same_change(o, t, ours_lines, theirs_lines) => result.push(MergedChunk::Both(*o)),
            (os, ts) => {
                let (o, t) = (coalesce(os)?, coalesce(ts)?);
                result.push(if same_change(&o, &t, ours_lines, theirs_lines) {
                    MergedChunk::Both(o)
                } else {
                    MergedChunk::Conflict(o, t)
                });
            }
        }
    }
    Some(result)
}

fn high_mark(a: &[Hunk], b: &[Hunk]) -> usize {
    a.iter().chain(b).map(|h| h.old_end).max().unwrap_or(0)
}

/// One hunk spanning all of `hunks` (gaps between them on the side are included).
fn coalesce(hunks: &[Hunk]) -> Option<Hunk> {
    Some(Hunk {
        kind: EditType::Replace,
        old_start: hunks.iter().map(|h| h.old_start).min()?,
        old_end: hunks.iter().map(|h| h.old_end).max()?,
        new_start: hunks.iter().map(|h| h.new_start).min()?,
        new_end: hunks.iter().map(|h| h.new_end).max()?,
    })
}

/// Both sides replaced the same base lines with the same lines.
fn same_change(ours: &Hunk, theirs: &Hunk, ours_lines: &[&str], theirs_lines: &[&str]) -> bool {
    ours.old_start == theirs.old_start
        && ours.old_end == theirs.old_end
        && ours.new_end.checked_sub(ours.new_start) == theirs.new_end.checked_sub(theirs.new_start)
        && ours_lines
            .get(ours.new_start..ours.new_end)
            .is_some_and(|o| theirs_lines.get(theirs.new_start..theirs.new_end) == Some(o))
}

fn apply_merged_chunks(base_lines: &[&str], ours_lines: &[&str], theirs_lines: &[&str], chunks: Vec<MergedChunk>) -> Option<EditorMerge> {
    let mut result: Vec<&str> = Vec::new();
    let mut out: Vec<EditorChunk> = Vec::new();
    let mut last_line = 0;

    for chunk in chunks {
        let (hunk, lines) = match chunk {
            MergedChunk::Ours(h) | MergedChunk::Both(h) => (h, ours_lines),
            MergedChunk::Theirs(h) => (h, theirs_lines),
            MergedChunk::Conflict(o, t) => {
                let (start, end) = (o.old_start.min(t.old_start), o.old_end.max(t.old_end));
                result.extend(base_lines.get(last_line.min(start)..start)?);
                let merged_start = result.len();
                result.extend(base_lines.get(start..end)?);
                out.push(EditorChunk {
                    base: start..end,
                    ours: side_range(&o, start, end)?,
                    theirs: side_range(&t, start, end)?,
                    merged: merged_start..result.len(),
                });
                last_line = end;
                continue;
            }
        };
        result.extend(base_lines.get(last_line.min(hunk.old_start)..hunk.old_start)?);
        result.extend(lines.get(hunk.new_start..hunk.new_end)?);
        last_line = hunk.old_end;
    }
    result.extend(base_lines.get(last_line.min(base_lines.len())..)?);
    Some(EditorMerge {
        merged: result.join("\n"),
        chunks: out,
    })
}

/// A side's lines for the conflict over base `start..end`: its own change, widened by the base lines around it
/// that only the other side changed (which this side kept as they were). diff3's skew correction.
fn side_range(hunk: &Hunk, start: usize, end: usize) -> Option<Range<usize>> {
    let from = hunk.new_start.checked_sub(hunk.old_start.checked_sub(start)?)?;
    Some(from..hunk.new_end + end.checked_sub(hunk.old_end)?)
}

/// Ranges in bounds and in order, and the template holds each chunk's base lines.
fn is_consistent(merge: &EditorMerge, base: &[&str], ours: &[&str], theirs: &[&str]) -> bool {
    let merged = split_pieces(&merge.merged);
    let mut prev_end = 0;
    merge.chunks.iter().all(|c| {
        let ok = c.merged.start >= prev_end
            && ours.get(c.ours.clone()).is_some()
            && theirs.get(c.theirs.clone()).is_some()
            && c.base.start <= c.base.end
            && base.get(c.base.clone()).is_some_and(|b| merged.get(c.merged.clone()) == Some(b));
        prev_end = c.merged.end;
        ok
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(base: &str, ours: &str, theirs: &str) -> EditorMerge {
        editor_chunks(base, ours, theirs).unwrap_or_else(|| panic!("no layout"))
    }

    #[test]
    fn identical_inputs_have_no_chunks() {
        let m = layout("a\nb\n", "a\nb\n", "a\nb\n");
        assert!(m.chunks.is_empty());
        assert_eq!(m.merged, "a\nb\n");
    }

    #[test]
    fn one_line_conflict() {
        let m = layout("a\nb\nc\n", "a\nX\nc\n", "a\nY\nc\n");
        assert_eq!(
            m.chunks,
            vec![EditorChunk {
                base: 1..2,
                ours: 1..2,
                theirs: 1..2,
                merged: 1..2
            }]
        );
        assert_eq!(m.merged, "a\nb\nc\n");
    }

    #[test]
    fn chunk_starting_at_line_one_is_not_reported_at_zero() {
        // The old "+1 unless 0" conversion reported this chunk as starting at line 0.
        let m = layout("a\nb\n", "a\nX\n", "a\nY\n");
        assert_eq!(m.chunks.len(), 1);
        assert_eq!(
            (m.chunks[0].ours.start, m.chunks[0].merged.start, m.chunks[0].base.start),
            (1, 1, 1)
        );
    }

    #[test]
    fn change_of_the_last_line_without_newline_is_not_duplicated() {
        // WP6.1: delete+insert at the end of the script; the old code emitted the insertion twice.
        let m = layout("a\nb", "a\nB", "x\na\nb");
        assert!(m.chunks.is_empty());
        assert_eq!(m.merged, "x\na\nB");
    }

    #[test]
    fn several_hunks_on_one_side_do_not_panic() {
        // WP6.2: base "" / ours "x1" / theirs "" used to panic in `coalesce_hunks`.
        let m = layout("", "x1", "");
        assert!(m.chunks.is_empty());
        assert_eq!(m.merged, "x1");
    }

    #[test]
    fn large_rewrites_give_up_instead_of_blowing_up() {
        let base: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
        let ours: String = (0..20_000).map(|i| format!("ours {i}\n")).collect();
        assert!(editor_chunks(&base, &ours, &base).is_none());
    }
}
