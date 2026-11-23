//! The old engine's fixtures (`misc/test_files`, moved to `tests/fixtures/merge`) through **git's** layout
//! ([`merge_texts`], a port of `git merge-file --diff3`), checked against `git merge-file` and pinned. That layout
//! renders conflict markers ([`render_diff3`]) and is the merge editor's fallback.
//!
//! The merge editor itself uses the old app's layout; its expected values are the old suite's,
//! unchanged, in `merge_legacy_suite.rs`. The two layouts agree on every fixture except `complex_5_chunks` (xdiff
//! aligns theirs' conflicting lines at 204.., after a 144-line insertion, where the editor's diff pairs them
//! with the matching `analyze_conflicts` lines at 54..). (`mod_rs_edge_case` agrees once the editor layout's final
//! empty line, which `split_inclusive` does not have, is dropped.)
//!
//! Ranges are 0-based, end-exclusive line ranges (`split_inclusive('\n')`) in each file's own coordinates
//! (`base` = the real ancestor; `merged` = position in the auto-merged template).
#![allow(clippy::unwrap_used, clippy::expect_used)] // test-only helpers (clippy.toml covers #[test] fns only)

mod diff_support;

use gityas_git::merge::{merge_texts, render_diff3};

type R = (usize, usize);

/// (ours, theirs, base, merged) ranges per chunk.
fn check(name: &str, expected: &[(R, R, R, R)]) {
    let read = |f: &str| std::fs::read_to_string(format!("{}/tests/fixtures/merge/{name}/{f}.txt", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let (base, ours, theirs) = (read("ancestor"), read("ours"), read("theirs"));
    let (code, git_out) = diff_support::git_merge_file(&base, &ours, &theirs);
    assert_eq!(
        render_diff3(&base, &ours, &theirs, ["ours", "base", "theirs"]).unwrap(),
        git_out,
        "{name}: differs from git"
    );
    let m = merge_texts(&base, &ours, &theirs).unwrap();
    assert_eq!(m.conflicts.len() as i32, code, "{name}");
    let actual: Vec<_> = m
        .conflicts
        .iter()
        .map(|c| {
            (
                (c.ours.start, c.ours.end),
                (c.theirs.start, c.theirs.end),
                (c.base.start, c.base.end),
                (c.merged.start, c.merged.end),
            )
        })
        .collect();
    assert_eq!(actual, expected, "{name}");
}

#[test]
fn two_one_line_conflicts() {
    check(
        "2_one_line_conflicts",
        &[
            ((249, 250), (242, 243), (242, 243), (249, 250)),
            ((257, 258), (250, 251), (250, 251), (257, 258)),
        ],
    );
}

#[test]
fn complex_5_chunks() {
    // The editor layout differs here (merge_legacy_suite: theirs 54..58, 63..71, 83..89, 250..254, 519..527).
    check(
        "complex_5_chunks",
        &[
            ((36, 43), (204, 213), (36, 43), (204, 211)),
            ((48, 50), (224, 226), (48, 50), (222, 224)),
            ((54, 59), (231, 245), (54, 58), (229, 233)),
            ((64, 65), (250, 254), (63, 65), (238, 240)),
            ((161, 301), (537, 537), (161, 320), (523, 682)),
        ],
    );
}

#[test]
fn files_rs_conflicts() {
    check(
        "files_rs_conflicts",
        &[((9, 9), (2, 6), (2, 6), (9, 13)), ((149, 225), (140, 147), (140, 145), (153, 158))],
    );
}

#[test]
fn files_rs_simple_conflict() {
    check("files_rs_simple_conflict", &[((23, 30), (23, 34), (15, 25), (23, 33))]);
}

#[test]
fn merge_viewer_far_chunks() {
    check("merge_viewer_far_chunks", &[((107, 110), (80, 84), (80, 84), (107, 111))]);
}

#[test]
fn mod_rs_edge_case() {
    // The editor layout counts the empty line after ours' final newline: 0..4 in `split('\n')` lines.
    check("mod_rs_edge_case", &[((0, 3), (0, 1), (0, 1), (0, 1))]);
}

#[test]
fn auto_merge_two_same_chunks() {
    // The identical second change is auto-merged; one conflict remains.
    check("auto_merge_two_same_chunks", &[((3, 5), (3, 5), (3, 4), (3, 4))]);
}
