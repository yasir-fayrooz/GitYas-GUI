//! Headless UI tests for the merge editor: real conflicting merges made with git2, driven through the UI.

use std::path::{Path, PathBuf};

use egui_kittest::kittest::Queryable;
use gityas_core::models::{ConflictChunk, ConflictSide, ConflictSides, ConflictType, MergeConflict};

use super::{State, model, state_for};
use crate::app::Center;
use crate::testutil::{TestApp, commit_all, temp_repo, write_file};

/// `main` and `feature` both change lines 1 and 4 of `f.txt` (two conflicts); `main` is checked out and
/// `feature` is merged with conflicts left in the index (`MERGE_HEAD` set).
fn text_conflict() -> (tempfile::TempDir, git2::Repository) {
    let (dir, repo) = temp_repo();
    write_file(&repo, "f.txt", "a\nb\nc\nd\ne\n");
    commit_all(&repo, "base");
    diverge(
        &repo,
        |repo| write_file(repo, "f.txt", "a\nMAIN\nc\nd\nE-main\n"),
        |repo| write_file(repo, "f.txt", "a\nFEAT\nc\nd\nE-feat\n"),
    );
    (dir, repo)
}

/// Branches `feature` off HEAD, commits `on_main` on main and `on_feature` on feature, checks out main and
/// merges feature (leaving the conflicts).
fn diverge(repo: &git2::Repository, on_main: impl FnOnce(&git2::Repository), on_feature: impl FnOnce(&git2::Repository)) {
    let base = repo.head().and_then(|h| h.peel_to_commit()).unwrap_or_else(|e| panic!("{e}"));
    repo.branch("feature", &base, false).unwrap_or_else(|e| panic!("{e}"));
    on_main(repo);
    commit_all(repo, "main change");
    checkout(repo, "refs/heads/feature");
    on_feature(repo);
    commit_all(repo, "feature change");
    checkout(repo, "refs/heads/main");
    let feature = repo.find_reference("refs/heads/feature").unwrap_or_else(|e| panic!("{e}"));
    let annotated = repo.reference_to_annotated_commit(&feature).unwrap_or_else(|e| panic!("{e}"));
    repo.merge(&[&annotated], None, None).unwrap_or_else(|e| panic!("merge: {e}"));
    assert!(repo.index().map(|i| i.has_conflicts()).unwrap_or(false), "fixture must conflict");
}

fn checkout(repo: &git2::Repository, refname: &str) {
    repo.set_head(refname).unwrap_or_else(|e| panic!("{e}"));
    let mut opts = git2::build::CheckoutBuilder::new();
    opts.force().remove_untracked(true);
    repo.checkout_head(Some(&mut opts)).unwrap_or_else(|e| panic!("{e}"));
}

fn open(t: &mut TestApp, path: &str) {
    super::open(t.app(), PathBuf::from(path));
    t.settle_until("conflict loaded", |app| {
        app.tab()
            .is_some_and(|tab| !matches!(tab.merge.state, State::Idle | State::Loading))
    });
    t.harness.step();
}

/// Clicks are delivered over several frames (move, press, release); run enough of them.
fn steps(t: &mut TestApp) {
    for _ in 0..4 {
        t.harness.step();
    }
}

fn press(t: &mut TestApp, label: &str) {
    t.harness.get_by_label(label).click();
    steps(t);
}

fn press_contains(t: &mut TestApp, label: &str) {
    t.harness.get_by_label_contains(label).click();
    steps(t);
}

fn read(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// No conflict left; the path has a stage-0 entry unless it was deleted; the merge is still in progress.
fn assert_resolved(repo: &git2::Repository, rel: &str) {
    let mut index = repo.index().unwrap_or_else(|e| panic!("{e}"));
    index.read(true).unwrap_or_else(|e| panic!("{e}"));
    assert!(!index.has_conflicts(), "index still has conflicts");
    let on_disk = repo.workdir().is_some_and(|w| w.join(rel).exists());
    assert_eq!(
        index.get_path(Path::new(rel), 0).is_some(),
        on_disk,
        "stage-0 entry iff the file exists"
    );
    assert_eq!(repo.state(), git2::RepositoryState::Merge, "the merge stays in progress");
}

fn center(t: &mut TestApp) -> Center {
    t.app().tab().map(|tab| tab.center.clone()).unwrap_or_default()
}

fn model_text(t: &mut TestApp) -> String {
    t.app()
        .tab()
        .and_then(|tab| tab.merge.model())
        .map(|m| m.text().to_owned())
        .unwrap_or_default()
}

#[test]
fn choose_sides_per_chunk_and_save_resolves_the_file() {
    let mut t = TestApp::engine();
    let (dir, repo) = text_conflict();
    t.add_and_open(dir.path());
    open(&mut t, "f.txt");
    assert_eq!(center(&mut t), Center::Merge("f.txt".into()));
    assert_eq!(model_text(&mut t), "a\nb\nc\nd\ne\n");
    t.harness.get_by_label("2 conflicts");
    assert!(!t.app().layout.show_changes, "the Changes panel collapses in the merge editor");
    // Pane headers come from HEAD and MERGE_MSG ("Merge branch 'feature'").
    t.harness.get_by_label_contains("Ours: main");
    t.harness.get_by_label_contains("Theirs: feature");

    press(&mut t, "Use ours for conflict 1");
    press(&mut t, "Use theirs for conflict 2");
    assert_eq!(model_text(&mut t), "a\nMAIN\nc\nd\nE-feat\n");

    t.harness.get_by_label("Save").click();
    t.settle_until("back to the graph", |app| app.tab().is_some_and(|tab| tab.center == Center::Graph));
    assert_eq!(read(dir.path(), "f.txt"), "a\nMAIN\nc\nd\nE-feat\n");
    steps(&mut t);
    assert!(t.app().layout.show_changes, "and comes back after saving");
    assert_resolved(&repo, "f.txt");
    assert!(t.app().notify.toasts().any(|toast| toast.title == "Resolved f.txt"));
    t.settle_until("files refreshed", |app| {
        app.store.repo.as_ref().is_some_and(|r| {
            r.files
                .unstaged
                .iter()
                .chain(r.files.staged.iter())
                .all(|d| d.change != gityas_core::models::DeltaChange::Conflicted)
        })
    });
}

/// One of the Tauri app's real-file merge fixtures (`gityas-git/tests/fixtures/merge/<name>/{ancestor,ours,theirs}.txt`).
fn legacy_fixture(name: &str) -> [String; 3] {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../src-backend/gityas-git/tests/fixtures/merge")
        .join(name);
    ["ancestor", "ours", "theirs"].map(|f| read(&dir, &format!("{f}.txt")))
}

#[test]
fn mod_rs_edge_case_shows_the_new_last_line_in_the_chunk() {
    // Ours adds lines ending in a newline where base and theirs have none: as in the Tauri app
    // (ours 0..4 in `split('\n')` lines), the empty line after ours' final newline is part of the chunk.
    let [ancestor, ours, theirs] = legacy_fixture("mod_rs_edge_case");
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    write_file(&repo, "mod.rs", &ancestor);
    commit_all(&repo, "base");
    diverge(
        &repo,
        |repo| write_file(repo, "mod.rs", &ours),
        |repo| write_file(repo, "mod.rs", &theirs),
    );
    t.add_and_open(dir.path());
    open(&mut t, "mod.rs");
    let tab = t.app().tab();
    let m = tab.and_then(|tab| tab.merge.model()).unwrap_or_else(|| panic!("no model"));
    assert_eq!(m.chunk_count(), 1);
    assert_eq!(m.chunk_range(model::Pane::Ours, 0), Some(0..3));
    assert!(m.has_trailing_row(model::Side::Ours) && !m.has_trailing_row(model::Side::Theirs));
    assert_eq!(m.trailing_row_chunk(model::Side::Ours), Some(0));
    assert_eq!(m.trailing_row_chunk(model::Side::Theirs), None);
}

#[test]
fn real_merge_shows_the_old_apps_chunk_boundaries() {
    // `complex_5_chunks` through a real merge and the engine: the panes highlight the same lines as the Tauri app
    // (expected values from its suite, `gityas-git/tests/merge_legacy_suite.rs`), not git's xdiff layout.
    let [ancestor, ours, theirs] = legacy_fixture("complex_5_chunks");
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    write_file(&repo, "f.rs", &ancestor);
    commit_all(&repo, "base");
    diverge(
        &repo,
        |repo| write_file(repo, "f.rs", &ours),
        |repo| write_file(repo, "f.rs", &theirs),
    );
    t.add_and_open(dir.path());
    open(&mut t, "f.rs");
    t.harness.get_by_label("5 conflicts");

    let ranges = |t: &mut TestApp, pane: model::Pane| -> Vec<(usize, usize)> {
        let tab = t.app().tab();
        let m = tab.and_then(|tab| tab.merge.model()).unwrap_or_else(|| panic!("no model"));
        (0..m.chunk_count())
            .map(|i| m.chunk_range(pane, i).map_or((0, 0), |r| (r.start, r.end)))
            .collect()
    };
    let ours_old = vec![(36, 43), (48, 50), (54, 59), (64, 65), (263, 273)];
    let theirs_old = vec![(54, 58), (63, 71), (83, 89), (250, 254), (519, 527)];
    let template_old = vec![(54, 61), (66, 68), (80, 84), (245, 247), (512, 541)];
    assert_eq!(ranges(&mut t, model::Pane::Ours), ours_old);
    assert_eq!(ranges(&mut t, model::Pane::Theirs), theirs_old);
    assert_eq!(ranges(&mut t, model::Pane::Output), template_old);

    // Taking theirs for conflict 1 puts exactly theirs' lines 54..58 (the matching `analyze_conflicts` block) there.
    press(&mut t, "Use theirs for conflict 1");
    let lines: Vec<&str> = theirs.split_inclusive('\n').collect();
    let expected: String = lines[54..58].concat();
    let text = model_text(&mut t);
    let out: Vec<&str> = text.split_inclusive('\n').collect();
    assert_eq!(out[54..58].concat(), expected);
    assert!(expected.contains("pub fn analyze_conflicts(&mut self)"), "{expected}");
}

#[test]
fn per_line_chips_pick_single_lines_and_save() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    write_file(&repo, "f.txt", "a\nb\nc\nd\n");
    commit_all(&repo, "base");
    diverge(
        &repo,
        |repo| write_file(repo, "f.txt", "a\nM1\nM2\nd\n"),
        |repo| write_file(repo, "f.txt", "a\nF1\nF2\nd\n"),
    );
    t.add_and_open(dir.path());
    open(&mut t, "f.txt");
    assert_eq!(model_text(&mut t), "a\nb\nc\nd\n");

    press(&mut t, "Use theirs line 1 of conflict 1");
    press(&mut t, "Use ours line 2 of conflict 1");
    assert_eq!(model_text(&mut t), "a\nF1\nM2\nd\n", "lines go in click order");
    // The spine is partial; clicking it uses all of ours, below the theirs line.
    press(&mut t, "Use ours for conflict 1");
    assert_eq!(model_text(&mut t), "a\nF1\nM1\nM2\nd\n");
    // Undo goes back one click.
    press_contains(&mut t, "Undo");
    assert_eq!(model_text(&mut t), "a\nF1\nM2\nd\n");

    t.harness.get_by_label("Save").click();
    t.settle_until("back to the graph", |app| app.tab().is_some_and(|tab| tab.center == Center::Graph));
    assert_eq!(read(dir.path(), "f.txt"), "a\nF1\nM2\nd\n");
    assert_resolved(&repo, "f.txt");
}

#[test]
fn both_sides_use_all_undo_and_reset_through_the_ui() {
    let mut t = TestApp::engine();
    let (dir, _repo) = text_conflict();
    t.add_and_open(dir.path());
    open(&mut t, "f.txt");
    press(&mut t, "Use all of ours");
    press(&mut t, "Use theirs for conflict 1");
    assert_eq!(model_text(&mut t), "a\nMAIN\nFEAT\nc\nd\nE-main\n");
    press_contains(&mut t, "Undo");
    assert_eq!(model_text(&mut t), "a\nMAIN\nc\nd\nE-main\n");
    press_contains(&mut t, "Redo");
    assert_eq!(model_text(&mut t), "a\nMAIN\nFEAT\nc\nd\nE-main\n");
    press_contains(&mut t, "Reset");
    assert_eq!(model_text(&mut t), "a\nb\nc\nd\ne\n");
    // Navigator.
    t.harness.get_by_label("Conflict 1 of 2");
    press(&mut t, "Next conflict");
    t.harness.get_by_label("Conflict 2 of 2");
    // J / K when nothing has focus.
    t.harness.key_press(egui::Key::K);
    steps(&mut t);
    t.harness.get_by_label("Conflict 1 of 2");
}

#[test]
fn saving_with_unresolved_chunks_asks_first() {
    let mut t = TestApp::engine();
    let (dir, repo) = text_conflict();
    t.add_and_open(dir.path());
    open(&mut t, "f.txt");
    press(&mut t, "Use ours for conflict 1");
    press(&mut t, "Save");
    assert!(t.app().confirm.is_some(), "an unresolved chunk must be confirmed");
    press(&mut t, "Cancel");
    assert!(t.app().confirm.is_none());
    assert!(matches!(center(&mut t), Center::Merge(_)));
    press(&mut t, "Save");
    t.harness.get_by_label("Save anyway").click();
    t.settle_until("back to the graph", |app| app.tab().is_some_and(|tab| tab.center == Center::Graph));
    // The undecided chunk keeps the base line.
    assert_eq!(read(dir.path(), "f.txt"), "a\nMAIN\nc\nd\ne\n");
    assert_resolved(&repo, "f.txt");
}

#[test]
fn closing_with_unsaved_changes_asks_and_the_file_stays_conflicted() {
    let mut t = TestApp::engine();
    let (dir, repo) = text_conflict();
    t.add_and_open(dir.path());
    open(&mut t, "f.txt");
    press(&mut t, "Use ours for conflict 1");
    press(&mut t, "Close merge editor");
    assert!(t.app().confirm.is_some());
    press(&mut t, "Discard");
    assert_eq!(center(&mut t), Center::Graph);
    assert!(t.app().tab().is_some_and(|tab| tab.merge.path().is_none()));
    assert!(repo.index().map(|i| i.has_conflicts()).unwrap_or(false));
    // A clean editor closes without asking (Esc).
    open(&mut t, "f.txt");
    t.harness.key_press(egui::Key::Escape);
    steps(&mut t);
    assert_eq!(center(&mut t), Center::Graph);
}

#[test]
fn typing_in_the_output_is_saved_and_undoable() {
    let mut t = TestApp::engine();
    let (dir, _repo) = text_conflict();
    t.add_and_open(dir.path());
    open(&mut t, "f.txt");
    press(&mut t, "Use ours for conflict 1");
    press(&mut t, "Use ours for conflict 2");
    let output = t.harness.get_by_label("Merge output");
    output.focus();
    t.harness.step();
    // Select all, then type a replacement.
    t.harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    t.harness.step();
    t.harness.get_by_label("Merge output").type_text("hand\n");
    t.harness.step();
    assert_eq!(model_text(&mut t), "hand\n");
    t.harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
    t.harness.step();
    assert_eq!(model_text(&mut t), "a\nMAIN\nc\nd\nE-main\n");
}

#[test]
fn delete_modify_conflict_is_resolved_from_the_action_card() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    write_file(&repo, "gone.txt", "one\ntwo\n");
    commit_all(&repo, "base");
    diverge(
        &repo,
        |repo| {
            let path = repo.workdir().map(|w| w.join("gone.txt")).unwrap_or_default();
            std::fs::remove_file(path).unwrap_or_else(|e| panic!("{e}"));
        },
        |repo| write_file(repo, "gone.txt", "one\ntwo\nthree\n"),
    );
    t.add_and_open(dir.path());
    open(&mut t, "gone.txt");
    t.harness.get_by_label("Action needed");
    t.harness.get_by_label("gone.txt was deleted on main and modified on feature.");
    // Taking ours would delete the file: not offered (Delete file is).
    assert!(t.harness.query_by_label_contains("(main)").is_none());
    t.harness.get_by_label("Take base version");
    t.harness.get_by_label("Keep modified version (feature)").click();
    t.settle_until("back to the graph", |app| app.tab().is_some_and(|tab| tab.center == Center::Graph));
    assert_eq!(read(dir.path(), "gone.txt"), "one\ntwo\nthree\n");
    assert_resolved(&repo, "gone.txt");
}

#[test]
fn delete_file_is_confirmed() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    write_file(&repo, "gone.txt", "one\n");
    commit_all(&repo, "base");
    diverge(
        &repo,
        |repo| write_file(repo, "gone.txt", "one\nmain\n"),
        |repo| {
            let path = repo.workdir().map(|w| w.join("gone.txt")).unwrap_or_default();
            std::fs::remove_file(path).unwrap_or_else(|e| panic!("{e}"));
        },
    );
    t.add_and_open(dir.path());
    open(&mut t, "gone.txt");
    t.harness.get_by_label("gone.txt was modified on main and deleted on feature.");
    press(&mut t, "Delete file");
    assert!(t.app().confirm.is_some());
    assert!(dir.path().join("gone.txt").exists(), "nothing happens before confirming");
    assert_eq!(
        t.harness.get_all_by_label("Delete file").count(),
        2,
        "card button + confirmation button"
    );
    // Enter confirms (the confirmation's primary button).
    t.harness.key_press(egui::Key::Enter);
    t.settle_until("back to the graph", |app| app.tab().is_some_and(|tab| tab.center == Center::Graph));
    assert!(!dir.path().join("gone.txt").exists());
    assert_resolved(&repo, "gone.txt");
}

#[test]
fn binary_conflict_uses_the_action_card() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    write_file(&repo, "img.bin", "\0base");
    commit_all(&repo, "base");
    diverge(
        &repo,
        |repo| write_file(repo, "img.bin", "\0main"),
        |repo| write_file(repo, "img.bin", "\0feature"),
    );
    t.add_and_open(dir.path());
    open(&mut t, "img.bin");
    t.harness.get_by_label_contains("is a binary file");
    t.harness.get_by_label("Take base version");
    t.harness.get_by_label("Take version from feature").click();
    t.settle_until("back to the graph", |app| app.tab().is_some_and(|tab| tab.center == Center::Graph));
    assert_eq!(read(dir.path(), "img.bin"), "\0feature");
    assert_resolved(&repo, "img.bin");
    assert!(t.app().notify.toasts().any(|toast| toast.title.contains("version from feature")));
}

#[test]
fn a_file_resolved_elsewhere_shows_no_longer_in_conflict() {
    let mut t = TestApp::engine();
    let (dir, _repo) = text_conflict();
    t.add_and_open(dir.path());
    open(&mut t, "f.txt");
    let engine = t.app().bridge.engine().clone();
    t.block_on(engine.resolve_conflict("f.txt".into(), "x\n".into()))
        .unwrap_or_else(|e| panic!("{e}"));
    t.settle_until("gone", |app| app.tab().is_some_and(|tab| matches!(tab.merge.state, State::Gone)));
    t.harness.step();
    t.harness.get_by_label("No longer in conflict");
    press(&mut t, "Close");
    assert_eq!(center(&mut t), Center::Graph);
}

#[test]
fn opening_a_path_that_is_not_conflicted() {
    let mut t = TestApp::engine();
    let (dir, _repo) = text_conflict();
    t.add_and_open(dir.path());
    open(&mut t, "README.md");
    t.harness.get_by_label("No longer in conflict");
}

#[test]
fn mock_app_shows_the_load_error() {
    let mut t = TestApp::mock();
    super::open(t.app(), PathBuf::from("src/lib.rs"));
    t.settle();
    t.harness.step();
    t.harness.get_by_label("Could not load the conflict");
    t.harness.get_by_label("Retry");
}

fn sample_conflict() -> MergeConflict {
    let text = |bytes| {
        Some(ConflictSide {
            mode: 0o100_644,
            bytes,
            kind: gityas_core::diff::DiffKind::Text,
        })
    };
    MergeConflict {
        ancestor: "fn main() {\n    old();\n}\n".into(),
        ours: "fn main() {\n    ours();\n    more();\n}\n".into(),
        theirs: "fn main() {\n    theirs();\n}\n".into(),
        merged: "fn main() {\n    old();\n}\n".into(),
        chunks: vec![ConflictChunk {
            conflict_type: ConflictType::Both,
            ours_start: 1,
            ours_end: 3,
            theirs_start: 1,
            theirs_end: 2,
            ancestor_start: 1,
            ancestor_end: 2,
            merged_start: 1,
            merged_end: 2,
        }],
        sides: ConflictSides {
            ancestor: text(20),
            ours: text(30),
            theirs: text(25),
        },
        ours_deleted: false,
        theirs_deleted: false,
        auto_resolved: false,
    }
}

fn inject(t: &mut TestApp, path: &str, conflict: MergeConflict) {
    let tab = t.app().tab_mut().unwrap_or_else(|| panic!("no tab"));
    tab.center = Center::Merge(path.into());
    tab.merge.path = Some(path.into());
    let syntax = super::view::SideSyntax::compute(std::path::Path::new(path), &conflict);
    tab.merge.state = state_for(conflict, syntax);
}

#[test]
fn layout_menu_applies_presets_and_the_panes_keep_working() {
    let mut t = TestApp::mock();
    inject(&mut t, "src/main.rs", sample_conflict());
    steps(&mut t);
    assert!(super::view::load_layout(&t.harness.ctx).same_shape(&super::dock::Node::default()));
    press(&mut t, "Layout");
    t.harness.get_by_label("Result pane");
    press(&mut t, "Three columns");
    let three = &super::dock::presets()[1].1;
    assert!(super::view::load_layout(&t.harness.ctx).same_shape(three));
    press_contains(&mut t, "Reset to default");
    assert!(super::view::load_layout(&t.harness.ctx).same_shape(&super::dock::Node::default()));
    press(&mut t, "Sides over result");
    // Esc closes the menu, not the editor; picking still works in the new arrangement.
    t.harness.key_press(egui::Key::Escape);
    steps(&mut t);
    assert!(t.harness.query_by_label("Result pane").is_none(), "menu closed");
    assert_eq!(center(&mut t), Center::Merge("src/main.rs".into()));
    press(&mut t, "Use theirs for conflict 1");
    assert_eq!(model_text(&mut t), "fn main() {\n    theirs();\n}\n");
}

#[test]
fn dragging_a_pane_header_docks_it_beside_another_pane() {
    use super::dock::Slot;
    let mut t = TestApp::mock();
    inject(&mut t, "src/main.rs", sample_conflict());
    steps(&mut t);
    let from = t.harness.get_by_label("Result").rect().center();
    let ours = t.harness.get_by_label_contains("Ours: ").rect();
    // Into the left edge of the ours pane, below its header.
    let to = egui::pos2(ours.left() + 8.0, ours.bottom() + 60.0);
    t.harness.hover_at(from);
    t.harness.drag_at(from);
    steps(&mut t);
    for k in 1..=5 {
        t.harness.hover_at(from + (to - from) * (k as f32 / 5.0));
        t.harness.step();
    }
    t.harness.drop_at(to);
    steps(&mut t);
    // Result and ours now share the top row; theirs spans the bottom.
    let area = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 600.0));
    let rects = super::view::load_layout(&t.harness.ctx).rects(area, 0.0);
    let at = |slot: Slot| {
        rects
            .iter()
            .find(|(s, _)| *s == slot)
            .map(|(_, r)| *r)
            .unwrap_or(egui::Rect::NOTHING)
    };
    let (result, ours, theirs) = (at(Slot::Result), at(Slot::Ours), at(Slot::Theirs));
    assert!(result.right() <= ours.left() && result.top() == ours.top(), "{rects:?}");
    assert!(theirs.top() >= ours.bottom() && theirs.width() == area.width(), "{rects:?}");
}

#[test]
fn closing_the_editor_brings_the_changes_panel_back() {
    let mut t = TestApp::mock();
    inject(&mut t, "src/main.rs", sample_conflict());
    steps(&mut t);
    assert!(!t.app().layout.show_changes);
    // The drawer still works inside the editor.
    press(&mut t, "Settings");
    steps(&mut t);
    assert!(t.app().layout.showing(crate::app::SidePage::Settings));
    press(&mut t, "Close merge editor");
    assert_eq!(center(&mut t), Center::Graph);
    assert!(t.app().layout.show_changes);
    assert!(
        t.app().layout.showing(crate::app::SidePage::Settings),
        "the drawer is left as it was"
    );
}

#[test]
fn invalid_engine_ranges_show_an_error_instead_of_panicking() {
    let mut t = TestApp::mock();
    let mut bad = sample_conflict();
    bad.chunks[0].ours_end = 99;
    inject(&mut t, "src/main.rs", bad);
    t.harness.step();
    t.harness.get_by_label("Could not load the conflict");
}

#[test]
#[ignore = "needs a GPU"]
fn snapshot_merge_editor_and_action_card() {
    let mut t = TestApp::mock_gpu();
    inject(&mut t, "src/main.rs", sample_conflict());
    t.harness.step();
    t.harness.step();
    t.snapshot("merge_editor");
    // One line of ours, then all of theirs: partial and full spines, number chips, a mixed result.
    t.harness.get_by_label("Use ours line 1 of conflict 1").click();
    t.harness.step();
    t.harness.get_by_label("Use theirs for conflict 1").click();
    t.harness.step();
    t.harness.step();
    t.snapshot("merge_editor_picked");
    // The layout menu over a three-column layout.
    t.harness.get_by_label("Layout").click();
    t.harness.step();
    t.harness.step();
    t.harness.get_by_label("Three columns").click();
    t.harness.step();
    t.harness.step();
    t.harness.step();
    t.snapshot("merge_editor_layout_menu");
    let mut deleted = sample_conflict();
    deleted.sides.ours = None;
    deleted.ours_deleted = true;
    inject(&mut t, "src/main.rs", deleted);
    t.harness.step();
    t.harness.step();
    t.snapshot("merge_action_card");
}

/// Snapshot names come from the preset names: "Three columns" → `merge_layout_three_columns`.
fn snapshot_name(prefix: &str, label: &str) -> String {
    format!("{prefix}_{}", label.to_lowercase().replace(' ', "_"))
}

fn settle_frames(t: &mut TestApp) {
    for _ in 0..3 {
        t.harness.step();
    }
}

#[test]
#[ignore = "needs a GPU"]
fn snapshot_merge_loading_error_and_gone() {
    let mut t = TestApp::mock_gpu();
    inject(&mut t, "src/main.rs", sample_conflict());
    t.app().tab_mut().unwrap_or_else(|| panic!("no tab")).merge.state = State::Loading;
    settle_frames(&mut t);
    t.snapshot("merge_loading");
    t.app().tab_mut().unwrap_or_else(|| panic!("no tab")).merge.state = State::Gone;
    settle_frames(&mut t);
    t.snapshot("merge_gone");
    // The mock engine has no conflicts to load.
    super::open(t.app(), PathBuf::from("src/lib.rs"));
    t.settle();
    settle_frames(&mut t);
    t.snapshot("merge_load_error");
}

#[test]
#[ignore = "needs a GPU"]
fn snapshot_merge_editor_layouts() {
    let mut t = TestApp::mock_gpu();
    inject(&mut t, "src/main.rs", sample_conflict());
    settle_frames(&mut t);
    for (name, _) in super::dock::presets() {
        press(&mut t, "Layout");
        press(&mut t, name);
        // Esc closes the menu, not the editor.
        t.harness.key_press(egui::Key::Escape);
        settle_frames(&mut t);
        t.snapshot(snapshot_name("merge_layout", name));
    }
}

#[test]
#[ignore = "needs a GPU"]
fn snapshot_merge_action_cards() {
    let mut t = TestApp::mock_gpu();
    let mut theirs_deleted = sample_conflict();
    theirs_deleted.sides.theirs = None;
    theirs_deleted.theirs_deleted = true;
    let mut both_deleted = sample_conflict();
    both_deleted.sides.ours = None;
    both_deleted.sides.theirs = None;
    both_deleted.ours_deleted = true;
    both_deleted.theirs_deleted = true;
    let non_text = |kind: Option<gityas_core::diff::DiffKind>, mode: Option<u32>| {
        let mut c = sample_conflict();
        for side in [&mut c.sides.ancestor, &mut c.sides.ours, &mut c.sides.theirs]
            .into_iter()
            .flatten()
        {
            if let Some(kind) = kind {
                side.kind = kind;
            }
            if let Some(mode) = mode {
                side.mode = mode;
            }
        }
        c
    };
    use gityas_core::diff::DiffKind;
    let cards = [
        ("merge_action_card_theirs_deleted", theirs_deleted),
        ("merge_action_card_both_deleted", both_deleted),
        ("merge_action_card_binary", non_text(Some(DiffKind::Binary), None)),
        ("merge_action_card_non_utf8", non_text(Some(DiffKind::NonUtf8), None)),
        (
            "merge_action_card_too_large",
            non_text(Some(DiffKind::TooLarge { bytes: 12 * 1024 * 1024 }), None),
        ),
        ("merge_action_card_symlink", non_text(Some(DiffKind::Binary), Some(0o120_000))),
        ("merge_action_card_submodule", non_text(Some(DiffKind::Binary), Some(0o160_000))),
    ];
    for (name, conflict) in cards {
        inject(&mut t, "src/main.rs", conflict);
        settle_frames(&mut t);
        t.snapshot(name);
    }
    // Delete file asks first.
    let mut theirs_deleted = sample_conflict();
    theirs_deleted.sides.theirs = None;
    theirs_deleted.theirs_deleted = true;
    inject(&mut t, "src/main.rs", theirs_deleted);
    settle_frames(&mut t);
    press(&mut t, "Delete file");
    settle_frames(&mut t);
    t.snapshot("merge_delete_file_dialog");
}

#[test]
#[ignore = "needs a GPU"]
fn snapshot_merge_editor_dialogs() {
    let mut t = TestApp::mock_gpu();
    inject(&mut t, "src/main.rs", sample_conflict());
    settle_frames(&mut t);
    press(&mut t, "Save");
    settle_frames(&mut t);
    t.snapshot("merge_save_unresolved_dialog");
    press(&mut t, "Cancel");
    press(&mut t, "Use theirs for conflict 1");
    press(&mut t, "Close merge editor");
    settle_frames(&mut t);
    t.snapshot("merge_discard_dialog");
}

#[test]
#[ignore = "needs a GPU"]
fn snapshot_merge_views_light() {
    let mut t = TestApp::mock_gpu();
    t.app().store.settings.theme = gityas_core::models::Theme::Light;
    inject(&mut t, "src/main.rs", sample_conflict());
    settle_frames(&mut t);
    t.snapshot("merge_editor_light");
    press(&mut t, "Use ours line 1 of conflict 1");
    press(&mut t, "Use theirs for conflict 1");
    settle_frames(&mut t);
    t.snapshot("merge_editor_picked_light");
    let mut deleted = sample_conflict();
    deleted.sides.ours = None;
    deleted.ours_deleted = true;
    inject(&mut t, "src/main.rs", deleted);
    settle_frames(&mut t);
    t.snapshot("merge_action_card_light");
}

/// 120 lines with one conflict at line 61 whose lines run far past the pane's width.
fn far_wide_conflict() -> MergeConflict {
    let side = |mid: &str| {
        let mut s = String::new();
        for i in 0..120 {
            if i == 60 {
                s.push_str(&format!("let {mid} = \"{}\";\n", "wide ".repeat(80)));
            } else {
                s.push_str(&format!("line {i}\n"));
            }
        }
        s
    };
    let mut c = sample_conflict();
    c.ancestor = side("base");
    c.merged = side("base");
    c.ours = side("ours");
    c.theirs = side("theirs");
    c.chunks[0].ours_start = 60;
    c.chunks[0].ours_end = 61;
    c.chunks[0].theirs_start = 60;
    c.chunks[0].theirs_end = 61;
    c.chunks[0].ancestor_start = 60;
    c.chunks[0].ancestor_end = 61;
    c.chunks[0].merged_start = 60;
    c.chunks[0].merged_end = 61;
    c
}

#[test]
fn opening_a_file_centres_the_first_conflict_in_every_pane() {
    let mut t = TestApp::mock();
    inject(&mut t, "src/main.rs", far_wide_conflict());
    steps(&mut t);
    let ours = t.harness.get_by_label("Use ours for conflict 1").rect();
    let theirs = t.harness.get_by_label("Use theirs for conflict 1").rect();
    let ours_head = t.harness.get_by_label_contains("Ours: ").rect();
    let theirs_head = t.harness.get_by_label_contains("Theirs: ").rect();
    // Default layout: ours above theirs, so ours' body runs from its header down to theirs' header.
    assert!(
        theirs_head.top() > ours_head.bottom(),
        "layout changed: {ours_head:?} {theirs_head:?}"
    );
    let ours_body_mid = (ours_head.bottom() + theirs_head.top()) / 2.0;
    assert!(
        (ours.center().y - ours_body_mid).abs() <= 2.0,
        "ours not centred: {ours:?}, body middle {ours_body_mid}"
    );
    // Before the panes are laid out the chunk would land at the top; it must sit well below the header.
    assert!(theirs.top() > theirs_head.bottom() + 100.0, "theirs not centred: {theirs:?}");
}
