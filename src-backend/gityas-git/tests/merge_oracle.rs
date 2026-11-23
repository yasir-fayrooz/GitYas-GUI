//! Merge engine: property tests and `git merge-file --diff3` as the oracle.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test-only helpers (clippy.toml covers #[test] fns only)

mod diff_support;

use diff_support::*;
use gityas_git::merge::{conflict_from_texts, merge_texts, render_diff3};

const LABELS: [&str; 3] = ["ours", "base", "theirs"];

fn case(r: &mut Rng) -> (String, String, String) {
    let base = gen_lines(r, 10);
    let ours = mutate(r, &base);
    let theirs = mutate(r, &base);
    let trailing = r.below(3) != 0;
    // Sides may disagree on the trailing newline, too.
    let t2 = if r.below(5) == 0 { !trailing } else { trailing };
    (join(&base, "\n", trailing), join(&ours, "\n", trailing), join(&theirs, "\n", t2))
}

#[test]
fn oracle_random_lf_cases_match_git_merge_file_diff3_exactly() {
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut clean, mut conflicted) = (0, 0);
    for _ in 0..800 {
        let (b, o, t) = case(&mut r);
        let m = merge_texts(&b, &o, &t).unwrap();
        let (code, expected) = git_merge_file(&b, &o, &t);
        assert_eq!(m.conflicts.len() as i32, code, "conflict count b={b:?} o={o:?} t={t:?}");
        assert_eq!(render_diff3(&b, &o, &t, LABELS).unwrap(), expected, "b={b:?} o={o:?} t={t:?}");
        if m.is_clean() {
            assert_eq!(m.merged, expected);
            clean += 1;
        } else {
            conflicted += 1;
        }
    }
    // The generator must exercise both paths.
    assert!(clean > 150 && conflicted > 150, "clean={clean} conflicted={conflicted}");
}

#[test]
fn oracle_crlf_clean_merges_match_git() {
    let mut r = Rng(0x2545_f491_4f6c_dd1d);
    let mut checked = 0;
    for _ in 0..250 {
        let base = gen_lines(&mut r, 8);
        let (ours, theirs) = (mutate(&mut r, &base), mutate(&mut r, &base));
        let (b, o, t) = (join(&base, "\r\n", true), join(&ours, "\r\n", true), join(&theirs, "\r\n", true));
        let m = merge_texts(&b, &o, &t).unwrap();
        let (code, expected) = git_merge_file(&b, &o, &t);
        assert_eq!(m.conflicts.len() as i32, code);
        if code == 0 {
            assert_eq!(m.merged, expected);
            assert!(!m.merged.replace("\r\n", "").contains('\n'), "CRLF preserved");
            checked += 1;
        }
    }
    assert!(checked > 60);
}

#[test]
fn property_identities() {
    let mut r = Rng(0xdead_beef_cafe_f00d);
    for _ in 0..2000 {
        let (b, x, _) = case(&mut r);
        // merge(a, a, a) = a
        assert_eq!(merge_texts(&b, &b, &b).unwrap().merged, b);
        // merge(base, x, base) = x and merge(base, base, x) = x
        let m = merge_texts(&b, &x, &b).unwrap();
        assert!(m.is_clean());
        assert_eq!(m.merged, x);
        let m = merge_texts(&b, &b, &x).unwrap();
        assert!(m.is_clean());
        assert_eq!(m.merged, x);
        // merge(base, x, x) = x
        let m = merge_texts(&b, &x, &x).unwrap();
        assert!(m.is_clean(), "b={b:?} x={x:?}");
        assert_eq!(m.merged, x);
    }
}

#[test]
fn property_swapping_ours_and_theirs_is_symmetric() {
    let mut r = Rng(0x0123_4567_89ab_cdef);
    for _ in 0..2000 {
        let (b, o, t) = case(&mut r);
        let m1 = merge_texts(&b, &o, &t).unwrap();
        let m2 = merge_texts(&b, &t, &o).unwrap();
        let bases = |m: &gityas_git::merge::TextMerge| m.conflicts.iter().map(|c| c.base.clone()).collect::<Vec<_>>();
        assert_eq!(bases(&m1), bases(&m2), "b={b:?} o={o:?} t={t:?}");
        for (c1, c2) in m1.conflicts.iter().zip(&m2.conflicts) {
            assert_eq!((&c1.ours, &c1.theirs), (&c2.theirs, &c2.ours));
        }
        if m1.is_clean() {
            assert_eq!(m1.merged, m2.merged);
        }
    }
}

#[test]
fn property_ranges_are_in_bounds_and_template_holds_base_lines() {
    let mut r = Rng(0x1357_9bdf_2468_ace0);
    for _ in 0..2000 {
        let (b, o, t) = case(&mut r);
        let d = conflict_from_texts(&b, &o, &t).unwrap();
        let lines = |s: &str| s.split_inclusive('\n').map(str::to_owned).collect::<Vec<_>>();
        let (bl, ol, tl, ml) = (lines(&b), lines(&o), lines(&t), lines(&d.merged));
        assert_eq!(d.ancestor, b, "ancestor is the real base");
        assert_eq!(d.auto_resolved, d.chunks.is_empty());
        let mut prev_end = 0;
        for (c, mr) in d.chunks.iter().map(|c| (c, c.merged_range())) {
            assert!(c.ancestor_start <= c.ancestor_end && c.ancestor_end <= bl.len());
            assert!(c.ours_start <= c.ours_end && c.ours_end <= ol.len());
            assert!(c.theirs_start <= c.theirs_end && c.theirs_end <= tl.len());
            assert!(mr.start >= prev_end && mr.end <= ml.len());
            assert_eq!(ml[mr.clone()], bl[c.ancestor_start..c.ancestor_end]);
            prev_end = mr.end;
        }
    }
}

#[test]
fn unicode_and_empty_and_no_trailing_newline() {
    let m = merge_texts("", "", "").unwrap();
    assert!(m.is_clean() && m.merged.is_empty());
    let m = merge_texts("ä\nö\nü", "Ä\nö\nü", "ä\nö\nÜ").unwrap();
    assert_eq!(m.merged, "Ä\nö\nÜ");
    // One side adds the final newline, the other edits the first line: clean.
    let m = merge_texts("a\nb\nc\nd", "a\nb\nc\nd\n", "A\nb\nc\nd").unwrap();
    assert!(m.is_clean());
    assert_eq!(m.merged, "A\nb\nc\nd\n");
    // LF vs CRLF on the same line is a real difference (git agrees).
    let (code, _) = git_merge_file("a\nb\n", "a\r\nb\n", "A\nb\n");
    assert_eq!(merge_texts("a\nb\n", "a\r\nb\n", "A\nb\n").unwrap().conflicts.len() as i32, code);
}

#[test]
fn large_inputs_do_not_blow_up() {
    // The old hand-written Myers kept a full V-array per edit step: O((N+M)·D) memory, gigabytes here.
    let base: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
    let ours: String = (0..20_000).map(|i| format!("ours {i}\n")).collect();
    let theirs: String = (0..20_000)
        .map(|i| if i % 2 == 0 { format!("line {i}\n") } else { format!("t {i}\n") })
        .collect();
    let started = std::time::Instant::now();
    let m = merge_texts(&base, &ours, &theirs).unwrap();
    assert_eq!(m.conflicts.len(), 1);
    assert!(started.elapsed().as_secs() < 30);
}

#[test]
fn editor_layout_never_auto_resolves_what_git_conflicts_on() {
    // `conflict_from_texts` uses the old app's layout, but falls back to git's whenever that finds no conflict,
    // so a change git would not merge cleanly is always shown as a chunk.
    let mut r = Rng(0x0f0f_1234_5678_9abc);
    for _ in 0..2000 {
        let (b, o, t) = case(&mut r);
        let d = conflict_from_texts(&b, &o, &t).unwrap();
        let m = merge_texts(&b, &o, &t).unwrap();
        if !m.is_clean() {
            assert!(!d.auto_resolved, "b={b:?} o={o:?} t={t:?}");
        }
        if d.auto_resolved {
            assert_eq!(d.merged, m.merged, "a clean result is git's");
        }
    }
}

#[test]
fn large_rewrites_fall_back_to_gits_layout() {
    let base: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
    let ours: String = (0..20_000).map(|i| format!("ours {i}\n")).collect();
    let theirs: String = (0..20_000)
        .map(|i| if i % 2 == 0 { format!("line {i}\n") } else { format!("t {i}\n") })
        .collect();
    let started = std::time::Instant::now();
    let d = conflict_from_texts(&base, &ours, &theirs).unwrap();
    let m = merge_texts(&base, &ours, &theirs).unwrap();
    assert_eq!(d.merged, m.merged);
    assert_eq!(d.chunks.len(), m.conflicts.len());
    assert!(started.elapsed().as_secs() < 30);
}
