//! Merge-editor reference layouts, beyond the old app's suite in
//! `merge_legacy_suite.rs` (whose numbers stay unchanged). Expected values are pinned from a reference O(NP) +
//! diff3 merge on the same three files.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test-only helpers (clippy.toml covers #[test] fns only)

use gityas_git::merge::editor_chunks;

fn fixture(name: &str, file: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/merge/{name}/{file}.txt", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// 64-bit FNV-1a, to pin a long expected template without checking it in twice.
fn fnv1a(text: &str) -> u64 {
    text.bytes()
        .fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3))
}

#[test]
fn files_rs_insertion_before_a_conflict_is_merged() {
    // `files.rs` (Tauri app): ours replaced the `let load = |entry| …` closure with ~110 lines (early returns and two
    // new functions) and deleted the block after it, which theirs rewrote. Two shortest diffs tie on which `};` +
    // blank line of ours matches the base's; a plain Myers diff picks the first and puts ours' new lines inside one
    // 110-line conflict. The O(NP) diff picks the last: the new lines merge, and only the rewritten base block
    // conflicts (ours deleted it).
    let [base, ours, theirs] = ["ancestor", "ours", "theirs"].map(|f| fixture("files_rs_insert_before_conflict", f));
    let layout = editor_chunks(&base, &ours, &theirs).unwrap();
    let ranges: Vec<_> = layout
        .chunks
        .iter()
        .map(|c| (c.ours.clone(), c.theirs.clone(), c.base.clone(), c.merged.clone()))
        .collect();
    assert_eq!(
        ranges,
        [(9..10, 2..6, 2..6, 9..13), (343..343, 140..149, 140..150, 346..356)],
        "reference chunk ranges"
    );
    // The reference template, byte for byte (359 `split('\n')` lines).
    assert_eq!(layout.merged.split('\n').count(), 359);
    assert_eq!(fnv1a(&layout.merged), 0xfb17_4a50_e606_1ebf);
}
