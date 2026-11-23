//! Pure line-based three-way merge.
//!
//! This is a port of git's `xdl_do_merge` at level `XDL_MERGE_EAGER`, which is what `git merge-file --diff3`
//! runs, so `git merge-file --diff3 -p` is an exact test oracle. The per-side line diffs (base→ours,
//! base→theirs) come from libgit2's xdiff (`git2::Patch::from_buffers`, zero context), the same diff engine
//! git uses, so ambiguous alignments resolve the same way as in git.
//!
//! The model:
//! * A **line** is a `split_inclusive('\n')` piece: it keeps its terminator (`\n` or `\r\n`), and the last
//!   line may have none. So CRLF, a missing trailing newline and Unicode are all handled byte-exactly, and
//!   concatenating lines gives back the input.
//! * Changes on one side only are taken. Changes on both sides that **overlap or touch** (no unchanged base
//!   line between them) form a conflict, unless both sides made the identical change. Two insertions at
//!   the same base position are a conflict too. A one-sided change touching a conflict joins it.
//! * Every range is **0-based and end-exclusive** in lines of the file it refers to.

use std::ops::Range;

use gityas_core::{GitYasError, Result};

use crate::git_error;

/// One conflict found by [`merge_texts`]. All ranges are 0-based, end-exclusive line ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextConflict {
    /// Lines of the base (ancestor) file covered by the conflict. Empty when both sides inserted at the same spot.
    pub base: Range<usize>,
    pub ours: Range<usize>,
    pub theirs: Range<usize>,
    /// Where the conflict sits in [`TextMerge::merged`] (it holds the base lines there).
    pub merged: Range<usize>,
}

/// Result of [`merge_texts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMerge {
    /// Every non-conflicting change applied; each conflict region holds the **base** lines (a template the
    /// merge editor overwrites per chunk). Without conflicts this is the merge result (byte-identical to
    /// `git merge-file -p`).
    pub merged: String,
    pub conflicts: Vec<TextConflict>,
}

impl TextMerge {
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// Splits into lines that keep their terminator (the xdiff record model).
pub fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// A change in a line script: `base[i1..i1+chg1]` became `side[i2..i2+chg2]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Change {
    i1: i64,
    chg1: i64,
    i2: i64,
    chg2: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Conflict,
    Ours,
    Theirs,
}

/// `xdmerge_t`: `base[i0..+chg0]`, `ours[i1..+chg1]`, `theirs[i2..+chg2]`.
#[derive(Debug, Clone, Copy)]
struct Merge {
    mode: Mode,
    i0: i64,
    chg0: i64,
    i1: i64,
    chg1: i64,
    i2: i64,
    chg2: i64,
}

fn to_i64(n: usize) -> Result<i64> {
    i64::try_from(n).map_err(|_| GitYasError::InvalidInput("file has too many lines to merge".into()))
}

fn to_usize(n: i64) -> Result<usize> {
    usize::try_from(n).map_err(|_| GitYasError::Other(format!("merge engine produced a negative line index ({n})")))
}

/// Line script base→side from libgit2's xdiff (zero context, so one hunk = one change).
fn line_script(base: &str, side: &str) -> Result<Vec<Change>> {
    let mut opts = git2::DiffOptions::new();
    opts.context_lines(0).interhunk_lines(0).force_text(true);
    let patch = git2::Patch::from_buffers(base.as_bytes(), None, side.as_bytes(), None, Some(&mut opts)).map_err(git_error)?;
    let mut script = Vec::with_capacity(patch.num_hunks());
    for idx in 0..patch.num_hunks() {
        let (hunk, _) = patch.hunk(idx).map_err(git_error)?;
        // Unified-diff convention: a zero-length range's start is the line *before* it, so it is already the
        // 0-based insertion index; otherwise the start is 1-based.
        let start = |start: u32, lines: u32| i64::from(if lines == 0 { start } else { start.saturating_sub(1) });
        script.push(Change {
            i1: start(hunk.old_start(), hunk.old_lines()),
            chg1: i64::from(hunk.old_lines()),
            i2: start(hunk.new_start(), hunk.new_lines()),
            chg2: i64::from(hunk.new_lines()),
        });
    }
    Ok(script)
}

/// `xdl_append_merge`: coalesce with the previous entry when they touch in ours or theirs coordinates.
fn append_merge(merges: &mut Vec<Merge>, next: Merge) {
    if let Some(last) = merges.last_mut()
        && (next.i1 <= last.i1 + last.chg1 || next.i2 <= last.i2 + last.chg2)
    {
        if next.mode != last.mode {
            last.mode = Mode::Conflict;
        }
        last.chg0 = next.i0 + next.chg0 - last.i0;
        last.chg1 = next.i1 + next.chg1 - last.i1;
        last.chg2 = next.i2 + next.chg2 - last.i2;
        return;
    }
    merges.push(next);
}

fn slice<'a>(lines: &'a [&'a str], start: i64, len: i64) -> Result<&'a [&'a str]> {
    let start = to_usize(start)?;
    let end = start
        .checked_add(to_usize(len)?)
        .ok_or_else(|| GitYasError::Other("merge range overflow".into()))?;
    lines
        .get(start..end)
        .ok_or_else(|| GitYasError::Other(format!("merge range {start}..{end} outside a file of {} lines", lines.len())))
}

/// Three-way merge of whole texts. Never panics on any input; errors only if libgit2 fails.
pub fn merge_texts(base: &str, ours: &str, theirs: &str) -> Result<TextMerge> {
    let base_lines = split_lines(base);
    let ours_lines = split_lines(ours);
    let theirs_lines = split_lines(theirs);
    let s1 = line_script(base, ours)?;
    let s2 = line_script(base, theirs)?;
    let (n0, n1, n2) = (to_i64(base_lines.len())?, to_i64(ours_lines.len())?, to_i64(theirs_lines.len())?);

    let mut merges: Vec<Merge> = Vec::new();
    let (mut a, mut b) = (0usize, 0usize);
    while let (Some(&x), Some(&y)) = (s1.get(a), s2.get(b)) {
        if x.i1 + x.chg1 < y.i1 {
            // Ours only; theirs position mapped through theirs' next change offset.
            let i2 = y.i2 - y.i1 + x.i1;
            append_merge(
                &mut merges,
                Merge {
                    mode: Mode::Ours,
                    i0: x.i1,
                    chg0: x.chg1,
                    i1: x.i2,
                    chg1: x.chg2,
                    i2,
                    chg2: x.chg1,
                },
            );
            a += 1;
            continue;
        }
        if y.i1 + y.chg1 < x.i1 {
            let i1 = x.i2 - x.i1 + y.i1;
            append_merge(
                &mut merges,
                Merge {
                    mode: Mode::Theirs,
                    i0: y.i1,
                    chg0: y.chg1,
                    i1,
                    chg1: y.chg1,
                    i2: y.i2,
                    chg2: y.chg2,
                },
            );
            b += 1;
            continue;
        }
        let identical = x.i1 == y.i1
            && x.chg1 == y.chg1
            && x.chg2 == y.chg2
            && slice(&ours_lines, x.i2, x.chg2)? == slice(&theirs_lines, y.i2, y.chg2)?;
        if !identical {
            let off = x.i1 - y.i1;
            let ffo = off + x.chg1 - y.chg1;
            let (mut i0, mut i1, mut i2) = (x.i1, x.i2, y.i2);
            if off > 0 {
                i0 -= off;
                i1 -= off;
            } else {
                i2 += off;
            }
            let mut chg0 = x.i1 + x.chg1 - i0;
            let mut chg1 = x.i2 + x.chg2 - i1;
            let mut chg2 = y.i2 + y.chg2 - i2;
            if ffo < 0 {
                chg0 -= ffo;
                chg1 -= ffo;
            } else {
                chg2 += ffo;
            }
            append_merge(
                &mut merges,
                Merge {
                    mode: Mode::Conflict,
                    i0,
                    chg0,
                    i1,
                    chg1,
                    i2,
                    chg2,
                },
            );
        }
        let (e1, e2) = (x.i1 + x.chg1, y.i1 + y.chg1);
        if e1 >= e2 {
            b += 1;
        }
        if e2 >= e1 {
            a += 1;
        }
    }
    for x in s1.iter().skip(a) {
        let i2 = x.i1 + n2 - n0;
        append_merge(
            &mut merges,
            Merge {
                mode: Mode::Ours,
                i0: x.i1,
                chg0: x.chg1,
                i1: x.i2,
                chg1: x.chg2,
                i2,
                chg2: x.chg1,
            },
        );
    }
    for y in s2.iter().skip(b) {
        let i1 = y.i1 + n1 - n0;
        append_merge(
            &mut merges,
            Merge {
                mode: Mode::Theirs,
                i0: y.i1,
                chg0: y.chg1,
                i1,
                chg1: y.chg1,
                i2: y.i2,
                chg2: y.chg2,
            },
        );
    }

    // `xdl_fill_merge_buffer`: walk ours, replacing theirs-only regions and conflicts.
    let mut merged = String::with_capacity(ours.len().max(theirs.len()));
    let mut merged_len = 0usize;
    let mut conflicts = Vec::new();
    let push = |merged: &mut String, lines: &[&str]| {
        for line in lines {
            merged.push_str(line);
        }
        lines.len()
    };
    let mut i = 0i64;
    for m in &merges {
        merged_len += push(&mut merged, slice(&ours_lines, i, m.i1 - i)?);
        match m.mode {
            Mode::Ours => merged_len += push(&mut merged, slice(&ours_lines, m.i1, m.chg1)?),
            Mode::Theirs => merged_len += push(&mut merged, slice(&theirs_lines, m.i2, m.chg2)?),
            Mode::Conflict => {
                let start = merged_len;
                merged_len += push(&mut merged, slice(&base_lines, m.i0, m.chg0)?);
                conflicts.push(TextConflict {
                    base: to_usize(m.i0)?..to_usize(m.i0 + m.chg0)?,
                    ours: to_usize(m.i1)?..to_usize(m.i1 + m.chg1)?,
                    theirs: to_usize(m.i2)?..to_usize(m.i2 + m.chg2)?,
                    merged: start..merged_len,
                });
            }
        }
        i = m.i1 + m.chg1;
    }
    push(&mut merged, slice(&ours_lines, i, n1 - i)?);
    Ok(TextMerge { merged, conflicts })
}

fn range<'a>(lines: &'a [&'a str], r: Range<usize>) -> Result<&'a [&'a str]> {
    let len = lines.len();
    lines
        .get(r.clone())
        .ok_or_else(|| GitYasError::Other(format!("merge range {r:?} outside a file of {len} lines")))
}

/// Line terminator for conflict markers. Like `git merge-file`, markers are CRLF only when every input's first
/// line ends in CRLF; any LF-only or unterminated input makes them LF.
fn marker_eol(texts: [&str; 3]) -> &'static str {
    let all_crlf = texts
        .iter()
        .all(|t| t.find('\n').is_some_and(|i| i > 0 && t.as_bytes()[i - 1] == b'\r'));
    if all_crlf { "\r\n" } else { "\n" }
}

/// Renders a merge with `diff3`-style conflict markers, exactly like `git merge-file --diff3 -p -L <ours>
/// -L <base> -L <theirs>` for both LF and CRLF files (a side whose last line has no newline gets one before the
/// next marker; markers end with the inputs' line terminator). Useful for "save with markers" / external tools,
/// and as the oracle bridge.
pub fn render_diff3(base: &str, ours: &str, theirs: &str, labels: [&str; 3]) -> Result<String> {
    let merge = merge_texts(base, ours, theirs)?;
    let eol = marker_eol([ours, base, theirs]);
    let (base_lines, ours_lines, theirs_lines, merged_lines) = (
        split_lines(base),
        split_lines(ours),
        split_lines(theirs),
        split_lines(&merge.merged),
    );
    let mut out = String::with_capacity(merge.merged.len());
    let push_side = |out: &mut String, lines: &[&str]| {
        for line in lines {
            out.push_str(line);
        }
        if lines.last().is_some_and(|l| !l.ends_with('\n')) {
            out.push_str(eol);
        }
    };
    let mut pos = 0usize;
    for c in &merge.conflicts {
        for line in range(&merged_lines, pos..c.merged.start)? {
            out.push_str(line);
        }
        out.push_str(&format!("<<<<<<< {}{eol}", labels[0]));
        push_side(&mut out, range(&ours_lines, c.ours.clone())?);
        out.push_str(&format!("||||||| {}{eol}", labels[1]));
        push_side(&mut out, range(&base_lines, c.base.clone())?);
        out.push_str(&format!("======={eol}"));
        push_side(&mut out, range(&theirs_lines, c.theirs.clone())?);
        out.push_str(&format!(">>>>>>> {}{eol}", labels[2]));
        pos = c.merged.end;
    }
    for line in range(&merged_lines, pos..merged_lines.len())? {
        out.push_str(line);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_merge_of_disjoint_changes() -> Result<()> {
        let m = merge_texts("a\nb\nc\nd\ne\n", "A\nb\nc\nd\ne\n", "a\nb\nc\nd\nE\n")?;
        assert!(m.is_clean());
        assert_eq!(m.merged, "A\nb\nc\nd\nE\n");
        Ok(())
    }

    #[test]
    fn overlapping_changes_conflict_with_exact_ranges() -> Result<()> {
        let m = merge_texts("a\nb\nc\n", "a\nX\nc\n", "a\nY\nY2\nc\n")?;
        assert_eq!(
            m.conflicts,
            vec![TextConflict {
                base: 1..2,
                ours: 1..2,
                theirs: 1..3,
                merged: 1..2
            }]
        );
        assert_eq!(m.merged, "a\nb\nc\n");
        Ok(())
    }

    #[test]
    fn adjacent_changes_conflict_like_git() -> Result<()> {
        let m = merge_texts("a\nb\nc\n", "A\nb\nc\n", "a\nB\nc\n")?;
        assert_eq!(m.conflicts.len(), 1);
        assert_eq!(m.conflicts[0].base, 0..2);
        Ok(())
    }

    #[test]
    fn identical_changes_merge_cleanly() -> Result<()> {
        let m = merge_texts("a\nb\n", "a\nB\n", "a\nB\n")?;
        assert!(m.is_clean());
        assert_eq!(m.merged, "a\nB\n");
        Ok(())
    }

    #[test]
    fn insertions_at_same_point_conflict_with_empty_base() -> Result<()> {
        let m = merge_texts("a\nb\n", "a\nX\nb\n", "a\nY\nb\n")?;
        assert_eq!(
            m.conflicts,
            vec![TextConflict {
                base: 1..1,
                ours: 1..2,
                theirs: 1..2,
                merged: 1..1
            }]
        );
        Ok(())
    }

    #[test]
    fn empty_inputs() -> Result<()> {
        assert_eq!(merge_texts("", "", "")?.merged, "");
        assert_eq!(merge_texts("", "x\n", "")?.merged, "x\n");
        assert_eq!(merge_texts("", "", "y")?.merged, "y");
        assert_eq!(merge_texts("", "x\n", "y\n")?.conflicts.len(), 1);
        Ok(())
    }
}
