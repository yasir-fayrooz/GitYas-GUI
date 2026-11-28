//! Headless UI tests of the diff viewer against a real in-memory engine and temp repositories, plus GPU
//! snapshots (ignored in the sandbox).

use std::path::Path;
use std::sync::Arc;

use egui_kittest::kittest::Queryable;
use gityas_core::diff::{DiffKind, DiffTarget, FileDiff};
use gityas_core::models::{Delta, DeltaChange, DiffViewMode};
use gityas_engine::Refresh;

use super::*;
use crate::testutil::{TestApp, commit_all, temp_repo, write_file};

fn numbered(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

fn refresh(t: &mut TestApp) {
    let engine = t.app().bridge.engine().clone();
    t.block_on(engine.refresh(Refresh::FILES)).unwrap();
}

fn unstaged(path: &str, change: DeltaChange) -> DiffRequest {
    DiffRequest {
        target: DiffTarget::WorkingTree { staged: false },
        file: Delta::new(path, change),
    }
}

fn diff_tab(t: &mut TestApp) -> &DiffTab {
    &t.app().tab().unwrap().diff
}

/// Opens `request` and waits until its diff (or a guard / error) has arrived.
fn open_and_load(t: &mut TestApp, request: DiffRequest) {
    super::open(t.app(), request);
    t.settle_until("diff loaded", |app| {
        app.tab()
            .is_some_and(|tab| !tab.diff.loading && (tab.diff.loaded.is_some() || tab.diff.gone || tab.diff.error.is_some()))
    });
    t.harness.step();
}

fn loaded_diff(t: &mut TestApp) -> Arc<FileDiff> {
    diff_tab(t)
        .diff()
        .cloned()
        .unwrap_or_else(|| panic!("no diff: {:?}", diff_tab(t).error))
}

fn file_text(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap()
}

fn index_text(repo: &git2::Repository, rel: &str) -> String {
    let mut index = repo.index().unwrap();
    index.read(true).unwrap();
    let entry = index.get_path(Path::new(rel), 0).unwrap();
    String::from_utf8(repo.find_blob(entry.id).unwrap().content().to_vec()).unwrap()
}

/// A 30-line committed file changed at line 2 and line 28: two hunks.
fn two_hunk_repo(t: &mut TestApp) -> (tempfile::TempDir, git2::Repository, String, String) {
    let (dir, repo) = temp_repo();
    let old = numbered(30);
    write_file(&repo, "f.txt", &old);
    commit_all(&repo, "add f");
    t.add_and_open(dir.path());
    let new = old.replace("line 2\n", "LINE 2\n").replace("line 28\n", "LINE 28\n");
    write_file(&repo, "f.txt", &new);
    refresh(t);
    t.settle_until("f.txt listed", |app| app.store.repo.as_ref().is_some_and(|r| r.has_wip()));
    (dir, repo, old, new)
}

#[test]
fn opens_a_working_tree_diff_with_hunk_actions() {
    let mut t = TestApp::engine();
    let (_dir, _repo, _, _) = two_hunk_repo(&mut t);
    open_and_load(&mut t, unstaged("f.txt", DeltaChange::Modified));
    let diff = loaded_diff(&mut t);
    assert_eq!(diff.kind, DiffKind::Text);
    assert_eq!(diff.hunks.len(), 2);
    diff_model::tests::check_all_modes(&diff);
    assert_eq!(t.harness.query_all_by_label("Stage hunk").count(), 2);
    assert_eq!(t.harness.query_all_by_label("Discard hunk…").count(), 2);
    assert!(t.harness.query_by_label("Unstage hunk").is_none());
    t.harness.get_by_label("Stage file");
    t.harness.get_by_label("MODIFIED");
    // Default mode comes from the settings (Hunk).
    assert_eq!(t.app().store.settings.diff_view_mode, DiffViewMode::Hunk);
    // Intra-line highlights were computed with the diff.
    assert!(!t.app().tab().unwrap().diff.loaded.as_ref().unwrap().intraline.is_empty());
}

#[test]
fn switching_modes_persists_the_setting() {
    let mut t = TestApp::engine();
    let (_dir, _repo, _, _) = two_hunk_repo(&mut t);
    open_and_load(&mut t, unstaged("f.txt", DeltaChange::Modified));
    for (label, mode) in [
        ("Inline", DiffViewMode::Inline),
        ("Split", DiffViewMode::Split),
        ("Hunk", DiffViewMode::Hunk),
    ] {
        t.harness.get_by_label(label).click();
        t.settle_until("setting saved", |app| app.store.settings.diff_view_mode == mode);
        t.harness.step();
        let engine = t.app().bridge.engine().clone();
        assert_eq!(t.block_on(engine.settings()).unwrap().diff_view_mode, mode);
        assert_eq!(diff_tab(&mut t).mode_override, None, "the setting took over");
        let built_mode = diff_tab(&mut t).built.as_ref().map(|(k, _)| k.mode);
        assert_eq!(built_mode, Some(mode));
        // Hunk actions only in Hunk mode.
        let buttons = t.harness.query_all_by_label("Stage hunk").count();
        assert_eq!(buttons, if mode == DiffViewMode::Hunk { 2 } else { 0 }, "{label}");
    }
}

#[test]
fn stage_hunk_then_unstage_it() {
    let mut t = TestApp::engine();
    let (_dir, repo, old, _) = two_hunk_repo(&mut t);
    open_and_load(&mut t, unstaged("f.txt", DeltaChange::Modified));
    t.harness.query_all_by_label("Stage hunk").next().unwrap().click();
    t.settle_until("one hunk left", |app| {
        app.tab().and_then(|tab| tab.diff.diff()).is_some_and(|d| d.hunks.len() == 1)
    });
    let staged = index_text(&repo, "f.txt");
    assert!(staged.contains("LINE 2\n") && staged.contains("line 28\n"), "{staged}");

    // The staged side shows the first hunk with Unstage.
    let delta = t
        .app()
        .store
        .repo
        .as_ref()
        .unwrap()
        .files
        .find_staged(Path::new("f.txt"))
        .cloned()
        .unwrap();
    open_and_load(
        &mut t,
        DiffRequest {
            target: DiffTarget::WorkingTree { staged: true },
            file: delta,
        },
    );
    assert_eq!(loaded_diff(&mut t).hunks.len(), 1);
    assert!(t.harness.query_by_label("Stage hunk").is_none());
    t.harness.get_by_label("Unstage hunk").click();
    t.settle_until("staged diff gone", |app| app.tab().is_some_and(|tab| tab.diff.gone));
    assert_eq!(index_text(&repo, "f.txt"), old);
    t.harness.step();
    t.harness.get_by_label("No file changes");
}

#[test]
fn discard_hunk_asks_first() {
    let mut t = TestApp::engine();
    let (dir, _repo, _, _) = two_hunk_repo(&mut t);
    open_and_load(&mut t, unstaged("f.txt", DeltaChange::Modified));
    t.harness.query_all_by_label("Discard hunk…").last().unwrap().click();
    // Let the modal settle at its final position (first frames are a sizing pass) before clicking into it.
    for _ in 0..3 {
        t.harness.step();
    }
    assert!(t.app().confirm.is_some());
    assert!(
        file_text(dir.path(), "f.txt").contains("LINE 28"),
        "nothing discarded before confirming"
    );
    t.harness.get_by_label("Discard hunk").click();
    t.settle_until("one hunk left", |app| {
        app.tab().and_then(|tab| tab.diff.diff()).is_some_and(|d| d.hunks.len() == 1)
    });
    let text = file_text(dir.path(), "f.txt");
    assert!(text.contains("LINE 2\n") && text.contains("line 28\n"), "{text}");
}

#[test]
fn stale_hunk_shows_a_toast_and_refetches() {
    let mut t = TestApp::engine();
    let (_dir, repo, _, new) = two_hunk_repo(&mut t);
    open_and_load(&mut t, unstaged("f.txt", DeltaChange::Modified));
    // The file changes behind the viewer's back (no refresh: FSEvents is off in the sandbox anyway).
    write_file(&repo, "f.txt", &new.replace("LINE 2\n", "CHANGED 2\n"));
    t.harness.query_all_by_label("Stage hunk").next().unwrap().click();
    t.settle_until("refetched", |app| {
        app.tab()
            .and_then(|tab| tab.diff.diff())
            .is_some_and(|d| d.hunks.iter().any(|h| h.lines.iter().any(|l| l.text == "CHANGED 2")))
    });
    let toasts: Vec<String> = t.app().notify.toasts().map(|t| t.title.clone()).collect();
    assert!(toasts.iter().any(|t| t.contains("changed since it was shown")), "{toasts:?}");
    assert!(index_text(&repo, "f.txt").contains("line 2\n"), "nothing staged");
}

#[test]
fn file_leaving_the_list_shows_the_empty_state() {
    let mut t = TestApp::engine();
    let (_dir, repo, _, _) = two_hunk_repo(&mut t);
    open_and_load(&mut t, unstaged("f.txt", DeltaChange::Modified));
    // Staged outside the viewer.
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("f.txt")).unwrap();
    index.write().unwrap();
    refresh(&mut t);
    t.settle_until("gone", |app| app.tab().is_some_and(|tab| tab.diff.gone));
    t.harness.step();
    t.harness.get_by_label("No file changes");
}

#[test]
fn committing_the_shown_file_closes_the_viewer() {
    for staged in [true, false] {
        let mut t = TestApp::engine();
        let (_dir, repo, _, _) = two_hunk_repo(&mut t);
        let mut request = unstaged("f.txt", DeltaChange::Modified);
        if staged {
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("f.txt")).unwrap();
            index.write().unwrap();
            refresh(&mut t);
            t.settle_until("staged", |app| {
                app.store
                    .repo
                    .as_ref()
                    .is_some_and(|r| r.files.find_staged(Path::new("f.txt")).is_some())
            });
            request.target = DiffTarget::WorkingTree { staged: true };
        }
        open_and_load(&mut t, request);
        commit_all(&repo, "commit f.txt");
        refresh(&mut t);
        t.settle_until("viewer closed", |app| app.tab().is_some_and(|tab| tab.center == Center::Graph));
    }
}

#[test]
fn staging_the_last_file_closes_the_viewer() {
    let mut t = TestApp::engine();
    let (_dir, _repo, _, _) = two_hunk_repo(&mut t);
    open_and_load(&mut t, unstaged("f.txt", DeltaChange::Modified));
    t.harness.get_by_label("Stage file").click();
    t.settle_until("viewer closed", |app| app.tab().is_some_and(|tab| tab.center == Center::Graph));
}

#[test]
fn keyboard_navigates_hunks_and_esc_closes() {
    let mut t = TestApp::engine();
    let (_dir, _repo, _, _) = two_hunk_repo(&mut t);
    open_and_load(&mut t, unstaged("f.txt", DeltaChange::Modified));
    let press = |t: &mut TestApp, key: egui::Key, mods: egui::Modifiers| {
        t.harness.key_press_modifiers(mods, key);
        t.harness.step();
        t.harness.step();
    };
    press(&mut t, egui::Key::J, egui::Modifiers::NONE);
    assert_eq!(diff_tab(&mut t).focused_hunk, Some(0));
    press(&mut t, egui::Key::F7, egui::Modifiers::NONE);
    assert_eq!(diff_tab(&mut t).focused_hunk, Some(1));
    press(&mut t, egui::Key::J, egui::Modifiers::NONE);
    assert_eq!(diff_tab(&mut t).focused_hunk, Some(1), "stops at the last hunk");
    press(&mut t, egui::Key::K, egui::Modifiers::NONE);
    assert_eq!(diff_tab(&mut t).focused_hunk, Some(0));
    press(&mut t, egui::Key::F7, egui::Modifiers::SHIFT);
    assert_eq!(diff_tab(&mut t).focused_hunk, Some(0));
    press(&mut t, egui::Key::Escape, egui::Modifiers::NONE);
    assert_eq!(t.app().tab().unwrap().center, Center::Graph);
}

#[test]
fn commit_diff_is_read_only() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    write_file(&repo, "README.md", "hello\nworld\n");
    let oid = commit_all(&repo, "second");
    t.add_and_open(dir.path());
    let oid = gityas_core::Oid::from_bytes(oid.as_bytes()).unwrap();
    open_and_load(
        &mut t,
        DiffRequest {
            target: DiffTarget::Commit(oid),
            file: Delta::new("README.md", DeltaChange::Modified),
        },
    );
    let diff = loaded_diff(&mut t);
    assert_eq!(diff.hunks.len(), 1);
    for label in ["Stage hunk", "Discard hunk…", "Unstage hunk", "Stage file", "Unstage file"] {
        assert!(t.harness.query_by_label(label).is_none(), "{label} on a commit diff");
    }
    t.harness.get_by_label("Hunk");
}

#[test]
fn binary_file_shows_the_guard() {
    let mut t = TestApp::engine();
    let (dir, _repo) = temp_repo();
    t.add_and_open(dir.path());
    std::fs::write(dir.path().join("blob.bin"), [0u8, 1, 2, 0, 255, 0]).unwrap();
    refresh(&mut t);
    t.settle_until("listed", |app| app.store.repo.as_ref().is_some_and(|r| r.has_wip()));
    open_and_load(&mut t, unstaged("blob.bin", DeltaChange::Untracked));
    assert_eq!(loaded_diff(&mut t).kind, DiffKind::Binary);
    t.harness.get_by_label("Binary file not shown");
    assert!(t.harness.query_by_label("Stage hunk").is_none());
    // File-level staging still works for binary files.
    t.harness.get_by_label("Stage file");
}

#[test]
fn too_large_file_can_be_shown_anyway() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    t.add_and_open(dir.path());
    // Just over the engine's 5 MB default cap.
    let line = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde\n";
    let big: String = line.repeat(5 * 1024 * 1024 / line.len() + 10);
    write_file(&repo, "big.txt", &big);
    refresh(&mut t);
    t.settle_until("listed", |app| app.store.repo.as_ref().is_some_and(|r| r.has_wip()));
    open_and_load(&mut t, unstaged("big.txt", DeltaChange::Untracked));
    assert!(matches!(loaded_diff(&mut t).kind, DiffKind::TooLarge { .. }));
    t.harness.get_by_label_contains("File too large to display");
    t.harness.get_by_label("Show anyway").click();
    t.settle_until("shown", |app| {
        app.tab().and_then(|tab| tab.diff.diff()).is_some_and(|d| d.kind == DiffKind::Text)
    });
    t.harness.step();
    assert!(diff_tab(&mut t).limit.is_some());
    t.harness.get_all_by_label("Stage hunk").next().unwrap();
}

#[test]
fn non_utf8_file_offers_read_only_hunks() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    std::fs::write(dir.path().join("latin1.txt"), b"caf\xe9\nend\n").unwrap();
    commit_all(&repo, "latin1");
    t.add_and_open(dir.path());
    std::fs::write(dir.path().join("latin1.txt"), b"caf\xe9!\nend\n").unwrap();
    refresh(&mut t);
    t.settle_until("listed", |app| app.store.repo.as_ref().is_some_and(|r| r.has_wip()));
    open_and_load(&mut t, unstaged("latin1.txt", DeltaChange::Modified));
    assert_eq!(loaded_diff(&mut t).kind, DiffKind::NonUtf8);
    t.harness.get_by_label("File is not valid UTF-8");
    t.harness.get_by_label("Show hunks (read-only)").click();
    t.harness.step();
    t.harness.step();
    assert!(diff_tab(&mut t).show_non_utf8);
    assert!(t.harness.query_by_label("Stage hunk").is_none(), "read-only");
    assert!(diff_tab(&mut t).built.as_ref().is_some_and(|(k, _)| k.mode == DiffViewMode::Hunk));
}

#[cfg(unix)]
#[test]
fn mode_only_change_shows_the_mode_line() {
    use std::os::unix::fs::PermissionsExt;
    let mut t = TestApp::engine();
    let (dir, _repo) = temp_repo();
    t.add_and_open(dir.path());
    let path = dir.path().join("README.md");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    refresh(&mut t);
    t.settle_until("listed", |app| app.store.repo.as_ref().is_some_and(|r| r.has_wip()));
    open_and_load(&mut t, unstaged("README.md", DeltaChange::Modified));
    let diff = loaded_diff(&mut t);
    assert!(diff.mode_changed() && !diff.has_hunks());
    t.harness.get_by_label("File mode changed 100644 → 100755");
    assert!(t.harness.query_by_label("No file changes").is_none());
}

/// Model invariants on what the real engine produces for the awkward cases.
#[test]
fn model_holds_on_real_engine_diffs() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    write_file(&repo, "crlf.txt", "a\r\nb\r\nc\r\n");
    write_file(&repo, "nonl.txt", "x\ny");
    write_file(&repo, "gone.txt", "bye\nbye\n");
    write_file(&repo, "long.txt", &numbered(60));
    commit_all(&repo, "fixtures");
    t.add_and_open(dir.path());
    write_file(&repo, "crlf.txt", "a\r\nB\r\nc\r\n");
    write_file(&repo, "nonl.txt", "x\nz\nw");
    std::fs::remove_file(dir.path().join("gone.txt")).unwrap();
    write_file(
        &repo,
        "long.txt",
        &numbered(60).replace("line 1\n", "").replace("line 60\n", "line sixty"),
    );
    write_file(&repo, "new.txt", "fresh\n");
    write_file(&repo, "empty.txt", "");
    refresh(&mut t);
    t.settle_until("listed", |app| app.store.repo.as_ref().is_some_and(|r| r.files.unstaged.len() >= 6));
    let engine = t.app().bridge.engine().clone();
    for (path, change) in [
        ("crlf.txt", DeltaChange::Modified),
        ("nonl.txt", DeltaChange::Modified),
        ("gone.txt", DeltaChange::Deleted),
        ("long.txt", DeltaChange::Modified),
        ("new.txt", DeltaChange::Untracked),
        ("empty.txt", DeltaChange::Untracked),
    ] {
        let diff = t
            .block_on(engine.file_diff(DiffTarget::WorkingTree { staged: false }, Delta::new(path, change)))
            .unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(diff.kind, DiffKind::Text, "{path}");
        diff_model::tests::check_all_modes(&diff);
    }
}

#[test]
fn selection_copies_line_text() {
    let mut t = TestApp::engine();
    let (_dir, _repo, _, _) = two_hunk_repo(&mut t);
    open_and_load(&mut t, unstaged("f.txt", DeltaChange::Modified));
    let d = &mut t.app().tab_mut().unwrap().diff;
    // Hunk mode rows: header, line 1, -line 2, +LINE 2, line 3, ...
    d.selection = Some(TextSelection {
        column: Column::Unified,
        anchor: TextPos::new(3, usize::MAX),
        cursor: TextPos::new(0, 0),
    });
    let expected = [
        (CopyMode::WithChanges, Some("line 1\nLINE 2")),
        (CopyMode::ChangesAndRemovals, Some("line 1\nline 2\nLINE 2")),
        (CopyMode::RemovalsOnly, Some("line 1\nline 2")),
        (CopyMode::Unchanged, Some("line 1")),
        (CopyMode::Patch, Some(" line 1\n-line 2\n+LINE 2")),
    ];
    for (mode, want) in expected {
        assert_eq!(selection_text(d, mode).as_deref(), want, "unified {mode:?}");
    }
    assert_eq!(exact_text(d).as_deref(), Some("line 1\nline 2\nLINE 2"));
    // Partial first / last lines are cut at the selection.
    d.selection = Some(TextSelection {
        column: Column::Unified,
        anchor: TextPos::new(1, 2),
        cursor: TextPos::new(3, 3),
    });
    assert_eq!(selection_text(d, CopyMode::WithChanges).as_deref(), Some("ne 1\nLIN"));
    assert_eq!(selection_text(d, CopyMode::Patch).as_deref(), Some(" ne 1\n-line 2\n+LIN"));
    // Only part of a removed line: nothing to copy "with changes" (Cmd+C then copies the highlight).
    d.selection = Some(TextSelection {
        column: Column::Unified,
        anchor: TextPos::new(2, 0),
        cursor: TextPos::new(2, 4),
    });
    assert_eq!(selection_text(d, CopyMode::WithChanges), None);
    assert_eq!(exact_text(d).as_deref(), Some("line"));

    // Split: the mode-based copies use both sides of the selected rows, in unified order; the highlight is one side.
    d.ensure_built(DiffViewMode::Split);
    d.selection = Some(TextSelection {
        column: Column::Right,
        anchor: TextPos::new(0, 0),
        cursor: TextPos::new(1, usize::MAX),
    });
    for (mode, want) in expected {
        assert_eq!(selection_text(d, mode).as_deref(), want, "split {mode:?}");
    }
    assert_eq!(exact_text(d).as_deref(), Some("line 1\nLINE 2"));
    d.selection = Some(TextSelection {
        column: Column::Left,
        anchor: TextPos::new(1, 0),
        cursor: TextPos::new(1, 4),
    });
    assert_eq!(selection_text(d, CopyMode::RemovalsOnly).as_deref(), Some("line"));
    assert_eq!(selection_text(d, CopyMode::WithChanges).as_deref(), Some("LINE 2"));
    assert_eq!(exact_text(d).as_deref(), Some("line"));
}

#[test]
fn word_boundaries() {
    use super::word_at;
    assert_eq!(word_at("let foo_bar = 1;", 5), 4..11);
    assert_eq!(word_at("let foo_bar = 1;", 3), 3..4);
    assert_eq!(word_at("a == b", 2), 2..4);
    assert_eq!(word_at("", 0), 0..0);
    assert_eq!(word_at("abc", 99), 0..3);
}

/// Injects a diff into the active tab without the engine (mock app, GPU snapshots).
fn inject(t: &mut TestApp, request: DiffRequest, diff: FileDiff) {
    let intraline = word_diff::compute(&diff);
    let syntax = super::DiffSyntax::compute(&request.file, &diff);
    super::open(t.app(), request.clone());
    let tab = t.app().tab_mut().unwrap();
    tab.diff.reset_for(request);
    tab.diff.set_loaded(Loaded {
        diff: Arc::new(diff),
        intraline: Arc::new(intraline),
        syntax: Arc::new(syntax),
    });
}

fn sample_diff() -> FileDiff {
    use gityas_core::diff::{DiffLine, Hunk, HunkRef, LineOrigin};
    let old = "{\n  \"extends\": \"../tsconfig.json\",\n  \"compilerOptions\": {\n    \"types\": [\"@cloudflare/vitest-plugin/types\"]\n  },\n  \"include\": [\"./**/*.ts\"],\n  \"exclude\": []\n}\n";
    let new = old.replace(
        "[\"@cloudflare/vitest-plugin/types\"]",
        "[\"../worker-configuration.d.ts\", \"node\"]",
    );
    let line = |origin, o, n, text: &str| DiffLine {
        origin,
        old_lineno: o,
        new_lineno: n,
        text: text.into(),
    };
    FileDiff {
        old_text: Arc::from(old),
        new_text: Arc::from(new.as_str()),
        hunks: vec![Hunk {
            id: HunkRef {
                old_start: 1,
                old_lines: 7,
                new_start: 1,
                new_lines: 7,
                content_hash: 1,
            },
            header: "@@ -1,7 +1,7 @@".into(),
            lines: vec![
                line(LineOrigin::Context, Some(1), Some(1), "{"),
                line(LineOrigin::Context, Some(2), Some(2), "  \"extends\": \"../tsconfig.json\","),
                line(LineOrigin::Context, Some(3), Some(3), "  \"compilerOptions\": {"),
                line(
                    LineOrigin::Del,
                    Some(4),
                    None,
                    "    \"types\": [\"@cloudflare/vitest-plugin/types\"]",
                ),
                line(
                    LineOrigin::Add,
                    None,
                    Some(4),
                    "    \"types\": [\"../worker-configuration.d.ts\", \"node\"]",
                ),
                line(LineOrigin::Context, Some(5), Some(5), "  },"),
                line(LineOrigin::Context, Some(6), Some(6), "  \"include\": [\"./**/*.ts\"],"),
                line(LineOrigin::Context, Some(7), Some(7), "  \"exclude\": []"),
            ],
        }],
        kind: DiffKind::Text,
        old_mode: Some(0o100_644),
        new_mode: Some(0o100_644),
    }
}

#[test]
fn mock_app_renders_an_injected_diff_in_every_mode() {
    let mut t = TestApp::mock();
    t.harness.step();
    let request = DiffRequest {
        target: DiffTarget::WorkingTree { staged: true },
        file: Delta::new("test/tsconfig.json", DeltaChange::Modified),
    };
    diff_model::tests::check_all_modes(&sample_diff());
    inject(&mut t, request, sample_diff());
    for mode in [DiffViewMode::Hunk, DiffViewMode::Inline, DiffViewMode::Split] {
        t.app().tab_mut().unwrap().diff.mode_override = Some(mode);
        t.harness.step();
        t.harness.step();
        let rows = diff_tab(&mut t).built.as_ref().map(|(k, b)| (k.mode, b.len()));
        let expected = match mode {
            DiffViewMode::Hunk => 9,
            DiffViewMode::Inline => 9,
            DiffViewMode::Split => 8,
        };
        assert_eq!(rows, Some((mode, expected)));
    }
    t.harness.get_by_label("Unstage file");
}

/// Press, drag and release over the rows select text like a read-only editor; double-click selects a word and
/// Cmd+A everything.
#[test]
fn mouse_selects_text() {
    let mut t = TestApp::mock();
    t.harness.step();
    let request = DiffRequest {
        target: DiffTarget::WorkingTree { staged: true },
        file: Delta::new("test/tsconfig.json", DeltaChange::Modified),
    };
    inject(&mut t, request, sample_diff());
    t.app().tab_mut().unwrap().diff.mode_override = Some(DiffViewMode::Hunk);
    t.harness.run_steps(2);
    // Row 0 is the hunk header carrying the button; each row is ROW_HEIGHT tall. The gutter is left of the text.
    let header = t.harness.get_by_label("Unstage hunk").rect();
    let row_y = |i: usize| header.center().y + i as f32 * rows::ROW_HEIGHT;
    let x = header.left() - 200.0;
    let button = |pos, pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    let start = egui::pos2(x, row_y(1));
    let end = egui::pos2(x, row_y(3));
    t.harness.event(egui::Event::PointerMoved(start));
    t.harness.step();
    t.harness.event(button(start, true));
    t.harness.step();
    for k in 1..=4 {
        t.harness.event(egui::Event::PointerMoved(start.lerp(end, k as f32 / 4.0)));
        t.harness.step();
    }
    t.harness.event(button(end, false));
    t.harness.step();
    let d = diff_tab(&mut t);
    let sel = d.selection.expect("a selection");
    assert_eq!((sel.column, sel.anchor.row, sel.cursor.row), (Column::Unified, 1, 3));
    assert!(!d.dragging);
    let text = exact_text(d).expect("selected text");
    assert_eq!(text.lines().count(), 3, "{text:?}");

    // Double-click on the "extends" line selects one word (the harness clock advances more per step than the
    // default double-click delay).
    t.harness.ctx.options_mut(|o| o.input_options.max_double_click_delay = 1000.0);
    let word = egui::pos2(x, row_y(2));
    t.harness.event(egui::Event::PointerMoved(word));
    t.harness.step();
    for pressed in [true, false, true, false] {
        t.harness.event(button(word, pressed));
        t.harness.step();
    }
    let d = diff_tab(&mut t);
    let sel = d.selection.expect("a word selection");
    assert_eq!(sel.anchor.row, 2);
    let text = exact_text(d).unwrap_or_else(|| panic!("word: {sel:?}"));
    assert!(!text.is_empty() && !text.contains('\n'), "{text:?}");

    // Cmd+A selects all, Cmd+C copies the new version (no removed line).
    t.harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    t.harness.step();
    let d = diff_tab(&mut t);
    let all = selection_text(d, CopyMode::WithChanges).expect("all");
    assert!(all.contains("worker-configuration") && !all.contains("vitest-plugin"), "{all}");
}

#[test]
#[ignore = "needs a GPU"]
fn snapshot_diff_viewer_modes_and_guard() {
    let mut t = TestApp::mock_gpu();
    t.harness.step();
    let request = DiffRequest {
        target: DiffTarget::WorkingTree { staged: true },
        file: Delta::new("src/example-worker/test/tsconfig.json", DeltaChange::Modified),
    };
    inject(&mut t, request.clone(), sample_diff());
    for (mode, name) in [
        (DiffViewMode::Hunk, "diff_hunk"),
        (DiffViewMode::Inline, "diff_inline"),
        (DiffViewMode::Split, "diff_split"),
    ] {
        t.app().tab_mut().unwrap().diff.mode_override = Some(mode);
        t.harness.step();
        t.harness.step();
        t.snapshot(name);
    }
    t.app().store.settings.theme = gityas_core::models::Theme::Light;
    t.harness.step();
    t.harness.step();
    t.snapshot("diff_split_light");
    inject(&mut t, request, FileDiff::without_content(DiffKind::Binary));
    t.harness.step();
    t.snapshot("diff_binary_guard");
}

/// The file history view on the mock app: its History pane (the staged change selected) and its Blame pane.
#[test]
#[ignore = "needs a GPU"]
fn snapshot_file_history_and_blame() {
    use gityas_core::models::{BlameHunk, Commit, FileBlame, FileHistoryEntry};
    const NEWEST: i64 = 1_792_320_120;
    let mut t = TestApp::mock_gpu();
    t.harness.step();
    let path = "src/example-worker/test/tsconfig.json";
    let request = DiffRequest {
        target: DiffTarget::WorkingTree { staged: true },
        file: Delta::new(path, DeltaChange::Modified),
    };
    inject(&mut t, request.clone(), sample_diff());
    let commit = |key: &str, summary: &str, (name, email): (&str, &str), age_hours: i64| Commit {
        id: crate::mock::oid(key),
        parents: Vec::new(),
        author_name: name.into(),
        author_email: email.into(),
        author_time: NEWEST - age_hours * 3600,
        committer_time: NEWEST - age_hours * 3600,
        summary: summary.into(),
        description: String::new(),
        lossy: false,
    };
    let john = ("John Smith", "john.smith@example.com");
    let alex = ("Alex Chen", "alex@example.com");
    let commits = [
        commit("fix-store", "fix: exclude generated files from the worker tests", alex, 23),
        commit("scaffold", "feat: scaffold egui app", john, 36),
        commit("providers", "feat: bitbucket, ado, gitlab support", john, 187),
        commit("tags-feature", "chore: vitest config for the example worker", alex, 255),
        commit("ctx-menu", "feat: example worker", john, 446),
    ];
    let entries = commits
        .iter()
        .enumerate()
        .map(|(i, c)| FileHistoryEntry {
            commit: c.clone(),
            file: Delta::new(
                path,
                if i + 1 == commits.len() {
                    DeltaChange::Added
                } else {
                    DeltaChange::Modified
                },
            ),
        })
        .collect::<Vec<_>>();
    let text = sample_diff().new_text.to_string();
    let hunk = |c: Option<&Commit>, start: usize, lines: usize| match c {
        Some(c) => BlameHunk {
            commit: Some(c.id.clone()),
            author_name: c.author_name.clone(),
            author_time: c.author_time,
            summary: c.summary.clone(),
            path: path.into(),
            start,
            lines,
        },
        None => BlameHunk {
            commit: None,
            author_name: "Not committed yet".into(),
            author_time: 0,
            summary: String::new(),
            path: path.into(),
            start,
            lines,
        },
    };
    let blame = FileBlame {
        text,
        lossy: false,
        hunks: vec![
            hunk(Some(&commits[4]), 0, 3),
            hunk(None, 3, 1),
            hunk(Some(&commits[3]), 4, 1),
            hunk(Some(&commits[0]), 5, 2),
            hunk(Some(&commits[4]), 7, 1),
        ],
    };

    file_tools::open(t.app(), request.clone(), file_tools::Pane::History);
    file_tools::inject(t.app(), entries.clone(), None);
    for _ in 0..3 {
        t.harness.step();
    }
    t.snapshot("file_history");

    file_tools::open(t.app(), request, file_tools::Pane::Blame);
    file_tools::inject(t.app(), entries, Some(blame));
    for _ in 0..3 {
        t.harness.step();
    }
    t.snapshot("file_blame");
}

fn tools(t: &mut TestApp) -> &file_tools::FileTools {
    &t.app().tab().unwrap().file
}

fn oid(id: git2::Oid) -> gityas_core::Oid {
    gityas_core::Oid::from_hex(&id.to_string()).unwrap_or_else(|e| panic!("{e}"))
}

fn selected_target(t: &mut TestApp) -> Option<DiffTarget> {
    tools(t).selected.as_ref().map(|r| r.target.clone())
}

#[test]
fn file_history_opens_full_screen_with_its_commits_pinned_on_the_left() {
    use crate::app::{Center, SidePage};
    let mut t = TestApp::engine();
    let (dir, repo, _, _) = two_hunk_repo(&mut t);
    let newest = commit_all(&repo, "edit f");
    write_file(
        &repo,
        "f.txt",
        &file_text(dir.path(), "f.txt").replace(
            "line 10
", "LINE 10
",
        ),
    );
    refresh(&mut t);
    t.settle();
    let origin = unstaged("f.txt", DeltaChange::Modified);
    open_and_load(&mut t, origin.clone());

    // History: full screen (no side panels), the uncommitted version first and selected, then both commits.
    t.harness.get_by_label("History").click();
    t.harness.step();
    assert!(matches!(t.app().tab().map(|t| &t.center), Some(Center::File(o)) if *o == origin));
    t.settle_until("history loaded", |app| !app.bridge.is_running(file_tools::TASK_HISTORY));
    t.harness.step();
    assert!(!t.app().layout.showing(SidePage::Changes) && !t.app().layout.showing(SidePage::Branches));
    t.harness.get_by_label("Uncommitted changes");
    t.harness.get_by_label("edit f");
    t.harness.get_by_label("2 commits");
    assert_eq!(selected_target(&mut t), Some(origin.target.clone()));

    // Down selects the newest commit; its diff is shown on the right.
    t.harness.key_press(egui::Key::ArrowDown);
    t.harness.step();
    assert_eq!(selected_target(&mut t), Some(DiffTarget::Commit(oid(newest))));
    t.settle_until("commit diff loaded", |app| {
        app.tab().is_some_and(|tab| {
            !tab.diff.loading
                && tab
                    .diff
                    .request
                    .as_ref()
                    .is_some_and(|r| r.target == DiffTarget::Commit(oid(newest)))
        })
    });

    // Blame of the selected commit; the list stays.
    t.harness.get_by_label("Blame").click();
    t.harness.step();
    assert_eq!(tools(&mut t).pane, Pane::Blame);
    t.settle_until("blame loaded", |app| !app.bridge.is_running(file_tools::TASK_BLAME));
    t.harness.step();
    t.harness.get_by_label_contains("As of");
    t.harness.get_by_label("Uncommitted changes");

    // Esc (or ✕) goes back to the diff it was opened from, with the side panels.
    t.harness.key_press(egui::Key::Escape);
    t.harness.step();
    t.harness.step();
    assert!(matches!(t.app().tab().map(|t| &t.center), Some(Center::Diff(o)) if *o == origin));
    assert!(t.app().layout.showing(SidePage::Changes));

    // A deleted file: Blame picks the last commit that has it.
    std::fs::remove_file(dir.path().join("f.txt")).unwrap();
    refresh(&mut t);
    t.settle();
    open_and_load(&mut t, unstaged("f.txt", DeltaChange::Deleted));
    t.harness.get_by_label("Blame").click();
    t.harness.step();
    t.settle_until("history loaded", |app| !app.bridge.is_running(file_tools::TASK_HISTORY));
    t.harness.step();
    assert_eq!(selected_target(&mut t), Some(DiffTarget::Commit(oid(newest))));
    t.harness.get_by_label("Close file history").click();
    t.harness.step();
    assert!(matches!(t.app().tab().map(|t| &t.center), Some(Center::Diff(_))));
}
