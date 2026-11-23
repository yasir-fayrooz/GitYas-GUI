//! Intra-line (word-level) highlights, in-house (no `similar` crate).
//!
//! Each run of deleted lines in a hunk is paired positionally with the following run of added lines (the same
//! pairing Split mode shows side by side). For each pair the lines are tokenised (identifier runs, whitespace
//! runs, single other characters), the common prefix / suffix is trimmed and the middle is aligned with an LCS
//! table. Unmatched tokens become highlighted byte ranges. Lines with nothing in common except whitespace get no
//! highlight (the whole line is already marked), and very long lines (`MAX_CELLS`) are skipped to bound the cost.
//!
//! [`compute`] runs once per loaded diff; the viewer caches the result next to the diff, keyed by
//! `(hunk, line)`, so each side of a pair has its own ranges ("cached per (file, side)").

use std::collections::HashMap;
use std::ops::Range;

use gityas_core::diff::{FileDiff, Hunk, LineOrigin};

/// Upper bound of the LCS table (tokens × tokens) per line pair.
const MAX_CELLS: usize = 250_000;

/// Byte ranges of one line.
pub type Ranges = Vec<Range<usize>>;

/// Highlighted byte ranges per `(hunk, line)`; lines without highlights are absent.
pub type IntraLine = HashMap<(usize, usize), Vec<Range<usize>>>;

fn class(c: char) -> u8 {
    if c.is_alphanumeric() || c == '_' {
        0
    } else if c.is_whitespace() {
        1
    } else {
        2
    }
}

/// Byte ranges of the tokens of `s`.
pub fn tokens(s: &str) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut prev: Option<u8> = None;
    for (i, c) in s.char_indices() {
        let k = class(c);
        let end = i + c.len_utf8();
        match (prev, out.last_mut()) {
            (Some(p), Some(last)) if p == k && k != 2 => last.end = end,
            _ => out.push(i..end),
        }
        prev = Some(k);
    }
    out
}

/// Changed byte ranges of `old` and `new`, or `None` when the lines share nothing but whitespace (or are too
/// long to align).
pub fn changed_ranges(old: &str, new: &str) -> Option<(Ranges, Ranges)> {
    let a = tokens(old);
    let b = tokens(new);
    let tok_a = |i: usize| a.get(i).and_then(|r| old.get(r.clone())).unwrap_or("");
    let tok_b = |i: usize| b.get(i).and_then(|r| new.get(r.clone())).unwrap_or("");

    let mut prefix = 0;
    while prefix < a.len() && prefix < b.len() && tok_a(prefix) == tok_b(prefix) {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < a.len() - prefix && suffix < b.len() - prefix && tok_a(a.len() - 1 - suffix) == tok_b(b.len() - 1 - suffix) {
        suffix += 1;
    }
    let (n, m) = (a.len() - prefix - suffix, b.len() - prefix - suffix);
    if n.saturating_mul(m) > MAX_CELLS {
        return None;
    }

    // LCS over the middle tokens; table[i][j] = LCS length of a[i..] and b[j..] (flattened, (n+1) x (m+1)).
    let width = m + 1;
    let mut table = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            let here = if tok_a(prefix + i) == tok_b(prefix + j) {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
            table[i * width + j] = here;
        }
    }
    let mut keep_a = vec![false; n];
    let mut keep_b = vec![false; m];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if tok_a(prefix + i) == tok_b(prefix + j) {
            keep_a[i] = true;
            keep_b[j] = true;
            i += 1;
            j += 1;
        } else if table[(i + 1) * width + j] >= table[i * width + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }

    // Anything in common besides whitespace?
    let common_a = (0..a.len()).filter(|&k| k < prefix || k >= a.len() - suffix || keep_a[k - prefix]);
    let meaningful = common_a.into_iter().any(|k| !tok_a(k).trim().is_empty());
    if !meaningful {
        return None;
    }

    let ranges = |toks: &[Range<usize>], keep: &[bool]| {
        let mut out: Vec<Range<usize>> = Vec::new();
        for (k, kept) in keep.iter().enumerate() {
            if *kept {
                continue;
            }
            let Some(r) = toks.get(prefix + k) else { continue };
            match out.last_mut() {
                Some(last) if last.end == r.start => last.end = r.end,
                _ => out.push(r.clone()),
            }
        }
        out
    };
    Some((ranges(&a, &keep_a), ranges(&b, &keep_b)))
}

/// `(deleted line, added line)` index pairs of a hunk: each del run paired positionally with the add run after
/// it.
pub fn pairs(hunk: &Hunk) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut dels: Vec<usize> = Vec::new();
    let mut adds: Vec<usize> = Vec::new();
    let mut flush = |dels: &mut Vec<usize>, adds: &mut Vec<usize>| {
        out.extend(dels.iter().copied().zip(adds.iter().copied()));
        dels.clear();
        adds.clear();
    };
    for (l, line) in hunk.lines.iter().enumerate() {
        match line.origin {
            LineOrigin::Del => {
                if !adds.is_empty() {
                    flush(&mut dels, &mut adds);
                }
                dels.push(l);
            }
            LineOrigin::Add => adds.push(l),
            LineOrigin::NoNewlineAtEof => {}
            LineOrigin::Context => flush(&mut dels, &mut adds),
        }
    }
    flush(&mut dels, &mut adds);
    out
}

/// Highlights for every paired line of `diff`.
pub fn compute(diff: &FileDiff) -> IntraLine {
    let mut out = IntraLine::new();
    for (h, hunk) in diff.hunks.iter().enumerate() {
        for (d, a) in pairs(hunk) {
            let (Some(old), Some(new)) = (hunk.lines.get(d), hunk.lines.get(a)) else {
                continue;
            };
            if let Some((old_ranges, new_ranges)) = changed_ranges(&old.text, &new.text) {
                if !old_ranges.is_empty() {
                    out.insert((h, d), old_ranges);
                }
                if !new_ranges.is_empty() {
                    out.insert((h, a), new_ranges);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gityas_core::diff::{DiffKind, DiffLine, HunkRef};

    use super::*;

    fn sub<'a>(s: &'a str, ranges: &[Range<usize>]) -> Vec<&'a str> {
        ranges.iter().map(|r| &s[r.clone()]).collect()
    }

    #[test]
    fn tokenises_words_spaces_and_punctuation() {
        let s = "let x_1 = foo(\"é\");";
        assert_eq!(
            sub(s, &tokens(s)),
            ["let", " ", "x_1", " ", "=", " ", "foo", "(", "\"", "é", "\"", ")", ";"]
        );
        assert!(tokens("").is_empty());
    }

    #[test]
    fn highlights_only_the_changed_words() {
        let old = r#"    "types": ["@cloudflare/vitest-plugin/types"]"#;
        let new = r#"    "types": ["../worker-configuration.d.ts", "node"]"#;
        let (a, b) = changed_ranges(old, new).unwrap();
        assert!(sub(old, &a).concat().contains("cloudflare"));
        assert!(!sub(old, &a).concat().contains("types\":"), "{:?}", sub(old, &a));
        assert!(sub(new, &b).concat().contains("node"));

        let (a, b) = changed_ranges("foo(bar, baz)", "foo(bar, qux)").unwrap();
        assert_eq!(sub("foo(bar, baz)", &a), ["baz"]);
        assert_eq!(sub("foo(bar, qux)", &b), ["qux"]);

        // Pure insertion: nothing on the old side.
        let (a, b) = changed_ranges("a b", "a new b").unwrap();
        assert!(a.is_empty());
        assert_eq!(sub("a new b", &b), ["new "]);
    }

    #[test]
    fn unrelated_lines_and_identical_lines() {
        assert_eq!(changed_ranges("alpha", "omega"), None);
        assert_eq!(changed_ranges("  x", "  y"), None, "only whitespace in common");
        let (a, b) = changed_ranges("same", "same").unwrap();
        assert!(a.is_empty() && b.is_empty());
        assert_eq!(changed_ranges("", ""), None);
    }

    #[test]
    fn huge_lines_are_skipped() {
        let old: String = (0..1000).map(|i| format!("a{i} ")).collect();
        let new: String = (0..1000).map(|i| format!("b{i} ")).collect();
        assert_eq!(changed_ranges(&format!("x {old} y"), &format!("x {new} y")), None);
    }

    fn line(origin: LineOrigin, text: &str) -> DiffLine {
        DiffLine {
            origin,
            old_lineno: None,
            new_lineno: None,
            text: text.into(),
        }
    }

    #[test]
    fn pairs_runs_positionally() {
        use LineOrigin::*;
        let hunk = Hunk {
            id: HunkRef {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                content_hash: 0,
            },
            header: String::new(),
            lines: vec![
                line(Context, "c"),
                line(Del, "a1"),
                line(Del, "a2"),
                line(NoNewlineAtEof, ""),
                line(Add, "b1"),
                line(Context, "c"),
                line(Add, "x"),
                line(Del, "y"),
                line(Add, "z"),
            ],
        };
        assert_eq!(pairs(&hunk), [(1, 4), (7, 8)]);
        let diff = FileDiff {
            old_text: Arc::from(""),
            new_text: Arc::from(""),
            hunks: vec![Hunk {
                lines: vec![line(Del, "let a = 1;"), line(Add, "let a = 2;")],
                ..hunk
            }],
            kind: DiffKind::Text,
            old_mode: None,
            new_mode: None,
        };
        let intra = compute(&diff);
        assert_eq!(intra.get(&(0, 0)).map(Vec::as_slice), Some(std::slice::from_ref(&(8..9))));
        assert_eq!(intra.get(&(0, 1)).map(Vec::as_slice), Some(std::slice::from_ref(&(8..9))));
    }

    /// Benchmark: a 5k-line file with every line changed. `cargo test --release -- --ignored
    /// word_diff_benchmark --nocapture`.
    #[test]
    #[ignore = "benchmark"]
    fn word_diff_benchmark() {
        let lines = 5000;
        let mut hunk_lines = Vec::new();
        for i in 0..lines {
            hunk_lines.push(line(
                LineOrigin::Del,
                &format!("    let value_{i} = compute(alpha, beta, {i}) + other_call(gamma);"),
            ));
        }
        for i in 0..lines {
            hunk_lines.push(line(
                LineOrigin::Add,
                &format!("    let value_{i} = compute(alpha, delta, {}) + other_call(gamma, extra);", i * 2),
            ));
        }
        let diff = FileDiff {
            old_text: Arc::from(""),
            new_text: Arc::from(""),
            hunks: vec![Hunk {
                id: HunkRef {
                    old_start: 1,
                    old_lines: lines,
                    new_start: 1,
                    new_lines: lines,
                    content_hash: 0,
                },
                header: String::new(),
                lines: hunk_lines,
            }],
            kind: DiffKind::Text,
            old_mode: None,
            new_mode: None,
        };
        let start = std::time::Instant::now();
        let intra = compute(&diff);
        let took = start.elapsed();
        println!("word diff of {lines} line pairs: {took:?} ({} highlighted lines)", intra.len());
        assert_eq!(intra.len(), 2 * lines as usize);
    }
}
