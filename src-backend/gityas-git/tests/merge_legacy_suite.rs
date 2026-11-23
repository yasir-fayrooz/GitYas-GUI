//! The Tauri app's merge-conflict suite (`src-tauri/gityas-data-access/src/misc/tests.rs`), with its original
//! numbers: real-file fixtures whose chunk boundaries were checked by hand. The fixtures moved to
//! `tests/fixtures/merge`; only the call site changed. Do not edit the expected values to fit an engine change.
//!
//! Old field meanings (the old frontend read every range as 0-based, end-exclusive): `ours_*` / `theirs_*` are
//! lines of those files; `ancestor_*` are lines of the *template* (the merged text the editor starts from), which
//! the new API calls `merged_*`. Lines are `split('\n')` pieces, as in the old app (Monaco), so a file ending in
//! a newline has a final empty line (`mod_rs_edge_case`).
#![allow(clippy::unwrap_used, clippy::expect_used)] // test-only helpers (clippy.toml covers #[test] fns only)
#![allow(clippy::get_first)] // the old test bodies are kept verbatim

use gityas_git::merge::{conflict_from_texts, editor_chunks};

#[derive(Debug)]
struct ConflictChunk {
    ours_start: usize,
    ours_end: usize,
    theirs_start: usize,
    theirs_end: usize,
    ancestor_start: usize,
    ancestor_end: usize,
}

struct GitYasConflict {
    chunks: Vec<ConflictChunk>,
}

fn fixture(name: &str, file: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/merge/{name}/{file}.txt", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// The old `git_rs::files::get_conflicts_by_files`, on the restored layout. Also checks that what the editor
/// receives (`conflict_from_texts`, `split_inclusive` lines) is the same layout, minus a final empty line.
fn get_conflicts_by_files(ancestor: String, ours: String, theirs: String) -> GitYasConflict {
    let layout = editor_chunks(&ancestor, &ours, &theirs).expect("layout");
    let editor = conflict_from_texts(&ancestor, &ours, &theirs).unwrap();
    assert_eq!(editor.merged, layout.merged);
    assert_eq!(editor.chunks.len(), layout.chunks.len());
    let incl = |s: &str| s.split_inclusive('\n').count();
    for (e, l) in editor.chunks.iter().zip(&layout.chunks) {
        let clamp = |r: &std::ops::Range<usize>, n: usize| (r.start.min(n), r.end.min(n));
        assert_eq!((e.ours_start, e.ours_end), clamp(&l.ours, incl(&ours)));
        assert_eq!((e.theirs_start, e.theirs_end), clamp(&l.theirs, incl(&theirs)));
        assert_eq!((e.ancestor_start, e.ancestor_end), clamp(&l.base, incl(&ancestor)));
        assert_eq!((e.merged_start, e.merged_end), clamp(&l.merged, incl(&layout.merged)));
    }
    GitYasConflict {
        chunks: layout
            .chunks
            .iter()
            .map(|c| ConflictChunk {
                ours_start: c.ours.start,
                ours_end: c.ours.end,
                theirs_start: c.theirs.start,
                theirs_end: c.theirs.end,
                ancestor_start: c.merged.start,
                ancestor_end: c.merged.end,
            })
            .collect(),
    }
}

#[test]
fn two_one_line_conflicts_test() {
    let ancestor: String = fixture("2_one_line_conflicts", "ancestor");

    let ours: String = fixture("2_one_line_conflicts", "ours");

    let theirs: String = fixture("2_one_line_conflicts", "theirs");

    let gityas_conflict = get_conflicts_by_files(ancestor, ours, theirs);

    assert_eq!(gityas_conflict.chunks.len(), 2);

    //First conflict
    let conflict = gityas_conflict.chunks.get(0).unwrap();
    assert_eq!(conflict.ours_start, 249);
    assert_eq!(conflict.ours_end, 250);
    assert_eq!(conflict.theirs_start, 242);
    assert_eq!(conflict.theirs_end, 243);
    assert_eq!(conflict.ancestor_start, 249);
    assert_eq!(conflict.ancestor_end, 250);

    //Second conflict
    let conflict = gityas_conflict.chunks.get(1).unwrap();
    assert_eq!(conflict.ours_start, 257);
    assert_eq!(conflict.ours_end, 258);
    assert_eq!(conflict.theirs_start, 250);
    assert_eq!(conflict.theirs_end, 251);
    assert_eq!(conflict.ancestor_start, 257);
    assert_eq!(conflict.ancestor_end, 258);
}

#[test]
fn complex_5_chunks_test() {
    let ancestor: String = fixture("complex_5_chunks", "ancestor");

    let ours: String = fixture("complex_5_chunks", "ours");

    let theirs: String = fixture("complex_5_chunks", "theirs");

    let gityas_conflict = get_conflicts_by_files(ancestor, ours, theirs);

    assert_eq!(gityas_conflict.chunks.len(), 5);

    //First conflict
    let conflict = gityas_conflict.chunks.get(0).unwrap();
    assert_eq!(conflict.ours_start, 36);
    assert_eq!(conflict.ours_end, 43);
    assert_eq!(conflict.theirs_start, 54);
    assert_eq!(conflict.theirs_end, 58);
    assert_eq!(conflict.ancestor_start, 54);
    assert_eq!(conflict.ancestor_end, 61);

    //Second conflict
    let conflict = gityas_conflict.chunks.get(1).unwrap();
    assert_eq!(conflict.ours_start, 48);
    assert_eq!(conflict.ours_end, 50);
    assert_eq!(conflict.theirs_start, 63);
    assert_eq!(conflict.theirs_end, 71);
    assert_eq!(conflict.ancestor_start, 66);
    assert_eq!(conflict.ancestor_end, 68);

    //Third conflict
    let conflict = gityas_conflict.chunks.get(2).unwrap();
    assert_eq!(conflict.ours_start, 54);
    assert_eq!(conflict.ours_end, 59);
    assert_eq!(conflict.theirs_start, 83);
    assert_eq!(conflict.theirs_end, 89);
    assert_eq!(conflict.ancestor_start, 80);
    assert_eq!(conflict.ancestor_end, 84);

    //Fourth conflict
    let conflict = gityas_conflict.chunks.get(3).unwrap();
    assert_eq!(conflict.ours_start, 64);
    assert_eq!(conflict.ours_end, 65);
    assert_eq!(conflict.theirs_start, 250);
    assert_eq!(conflict.theirs_end, 254);
    assert_eq!(conflict.ancestor_start, 245);
    assert_eq!(conflict.ancestor_end, 247);

    //Fifth conflict
    let conflict = gityas_conflict.chunks.get(4).unwrap();
    assert_eq!(conflict.ours_start, 263);
    assert_eq!(conflict.ours_end, 273);
    assert_eq!(conflict.theirs_start, 519);
    assert_eq!(conflict.theirs_end, 527);
    assert_eq!(conflict.ancestor_start, 512);
    assert_eq!(conflict.ancestor_end, 541);
}

#[test]
fn files_rs_conflicts_test() {
    let ancestor: String = fixture("files_rs_conflicts", "ancestor");

    let ours: String = fixture("files_rs_conflicts", "ours");

    let theirs: String = fixture("files_rs_conflicts", "theirs");

    let gityas_conflict = get_conflicts_by_files(ancestor, ours, theirs);

    assert_eq!(gityas_conflict.chunks.len(), 2);

    //First conflict
    let conflict = gityas_conflict.chunks.get(0).unwrap();
    assert_eq!(conflict.ours_start, 9);
    assert_eq!(conflict.ours_end, 9);
    assert_eq!(conflict.theirs_start, 2);
    assert_eq!(conflict.theirs_end, 6);
    assert_eq!(conflict.ancestor_start, 9);
    assert_eq!(conflict.ancestor_end, 13);

    //Second conflict
    let conflict = gityas_conflict.chunks.get(1).unwrap();
    assert_eq!(conflict.ours_start, 149);
    assert_eq!(conflict.ours_end, 225);
    assert_eq!(conflict.theirs_start, 140);
    assert_eq!(conflict.theirs_end, 147);
    assert_eq!(conflict.ancestor_start, 153);
    assert_eq!(conflict.ancestor_end, 158);
}

#[test]
fn files_rs_simple_conflict_test() {
    let ancestor: String = fixture("files_rs_simple_conflict", "ancestor");

    let ours: String = fixture("files_rs_simple_conflict", "ours");

    let theirs: String = fixture("files_rs_simple_conflict", "theirs");

    let gityas_conflict = get_conflicts_by_files(ancestor, ours, theirs);

    assert_eq!(gityas_conflict.chunks.len(), 1);

    //First conflict
    let conflict = gityas_conflict.chunks.get(0).unwrap();
    assert_eq!(conflict.ours_start, 23);
    assert_eq!(conflict.ours_end, 30);
    assert_eq!(conflict.theirs_start, 23);
    assert_eq!(conflict.theirs_end, 34);
    assert_eq!(conflict.ancestor_start, 23);
    assert_eq!(conflict.ancestor_end, 33);
}

#[test]
fn merge_viewer_far_chunks_test() {
    let ancestor: String = fixture("merge_viewer_far_chunks", "ancestor");

    let ours: String = fixture("merge_viewer_far_chunks", "ours");

    let theirs: String = fixture("merge_viewer_far_chunks", "theirs");

    let gityas_conflict = get_conflicts_by_files(ancestor, ours, theirs);

    assert_eq!(gityas_conflict.chunks.len(), 1);

    //First conflict
    let conflict = gityas_conflict.chunks.get(0).unwrap();
    assert_eq!(conflict.ours_start, 107);
    assert_eq!(conflict.ours_end, 110);
    assert_eq!(conflict.theirs_start, 80);
    assert_eq!(conflict.theirs_end, 84);
    assert_eq!(conflict.ancestor_start, 107);
    assert_eq!(conflict.ancestor_end, 111);
}

#[test]
fn mod_rs_edge_case_test() {
    let ancestor: String = fixture("mod_rs_edge_case", "ancestor");

    let ours: String = fixture("mod_rs_edge_case", "ours");

    let theirs: String = fixture("mod_rs_edge_case", "theirs");

    let gityas_conflict = get_conflicts_by_files(ancestor, ours, theirs);

    assert_eq!(gityas_conflict.chunks.len(), 1);

    //First conflict
    let conflict = gityas_conflict.chunks.get(0).unwrap();
    assert_eq!(conflict.ours_start, 0);
    assert_eq!(conflict.ours_end, 4);
    assert_eq!(conflict.theirs_start, 0);
    assert_eq!(conflict.theirs_end, 1);
    assert_eq!(conflict.ancestor_start, 0);
    assert_eq!(conflict.ancestor_end, 1);
}

#[test]
fn auto_merge_two_same_chunks_test() {
    let ancestor: String = fixture("auto_merge_two_same_chunks", "ancestor");

    let ours: String = fixture("auto_merge_two_same_chunks", "ours");

    let theirs: String = fixture("auto_merge_two_same_chunks", "theirs");

    let gityas_conflict = get_conflicts_by_files(ancestor, ours, theirs);

    assert_eq!(gityas_conflict.chunks.len(), 1);

    //First conflict
    let conflict = gityas_conflict.chunks.get(0).unwrap();
    assert_eq!(conflict.ours_start, 3);
    assert_eq!(conflict.ours_end, 5);
    assert_eq!(conflict.theirs_start, 3);
    assert_eq!(conflict.theirs_end, 5);
    assert_eq!(conflict.ancestor_start, 3);
    assert_eq!(conflict.ancestor_end, 4);
}
