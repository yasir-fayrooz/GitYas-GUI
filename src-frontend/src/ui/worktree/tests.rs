//! Headless UI tests of the WIP panel against real temp repositories (`TestApp::engine`) and the mock app.

use std::path::{Path, PathBuf};

use egui::{Key, Modifiers};
use egui_kittest::kittest::Queryable;
use gityas_core::models::{CommitSignature, DeltaChange, RepoOperation};
use gityas_engine::Refresh;

use super::model::Section;
use crate::app::{Center, GitYasApp};
use crate::testutil::{TestApp, commit_all, temp_repo, write_file};

/// The file row in the WIP panel. Clicking a row opens the diff viewer, whose header shows the same file name, so
/// pick the left-most match (the panel is on the left).
fn row<'a>(t: &'a TestApp, label: &'a str) -> egui_kittest::Node<'a> {
    t.harness
        .query_all_by_label(label)
        .min_by(|a, b| a.rect().left().total_cmp(&b.rect().left()))
        .unwrap_or_else(|| panic!("no row {label:?}"))
}

fn refresh_files(t: &mut TestApp) {
    let engine = t.app().bridge.engine().clone();
    t.block_on(engine.refresh(Refresh::FILES))
        .unwrap_or_else(|e| panic!("refresh: {e}"));
}

fn set_signature(t: &mut TestApp) {
    let engine = t.app().bridge.engine().clone();
    t.block_on(engine.update_commit_signature(CommitSignature::new("Ada Lovelace", "ada@example.com")))
        .unwrap_or_else(|e| panic!("signature: {e}"));
    t.settle_until("signature arrives", |app| app.store.signature().is_some());
}

/// Clicks a context-menu item like a user: the pointer reaches the popup one frame before the click (a click
/// in the same frame as the first hover of a popup is not delivered).
fn menu_click(t: &mut TestApp, label: &str) {
    t.harness.get_by_label(label).hover();
    t.harness.step();
    t.harness.get_by_label(label).click();
}

fn files(app: &GitYasApp) -> &gityas_core::models::FileState {
    &app.store.repo.as_ref().unwrap_or_else(|| panic!("no repo")).files
}

fn staged_paths(app: &GitYasApp) -> Vec<PathBuf> {
    files(app).staged.iter().map(|d| d.path.clone()).collect()
}

fn unstaged_paths(app: &GitYasApp) -> Vec<PathBuf> {
    files(app).unstaged.iter().map(|d| d.path.clone()).collect()
}

fn workdir(repo: &git2::Repository) -> PathBuf {
    repo.workdir().unwrap_or_else(|| panic!("bare")).to_path_buf()
}

fn wt(app: &mut GitYasApp) -> &mut super::WorktreeTab {
    &mut app.tab_mut().unwrap_or_else(|| panic!("no tab")).worktree
}

/// Opens a temp repository with `a.txt` (modified) and `new.txt` (untracked) as WIP.
fn dirty_repo(t: &mut TestApp) -> (tempfile::TempDir, git2::Repository) {
    let (dir, repo) = temp_repo();
    write_file(&repo, "a.txt", "one\n");
    commit_all(&repo, "add a");
    t.add_and_open(dir.path());
    write_file(&repo, "a.txt", "two\n");
    write_file(&repo, "new.txt", "new\n");
    refresh_files(t);
    t.settle_until("WIP listed", |app| files(app).unstaged.len() == 2);
    (dir, repo)
}

#[test]
fn stage_one_with_the_hover_button_then_stage_all() {
    let mut t = TestApp::engine();
    let (_dir, _repo) = dirty_repo(&mut t);
    t.harness.get_by_label("Unstaged files (2)");
    t.harness.get_by_label("Staged files (0)");

    row(&t, "a.txt").hover();
    t.harness.step();
    t.harness.get_by_label("Stage").click();
    t.settle_until("a.txt staged", |app| staged_paths(app) == [PathBuf::from("a.txt")]);
    assert_eq!(unstaged_paths(t.app()), [PathBuf::from("new.txt")]);
    // The refresh can land a frame before the stage command's completion, and the section buttons stay disabled
    // until it has run.
    t.settle();

    t.harness.get_by_label("Stage all changes").click();
    t.settle_until("everything staged", |app| {
        files(app).unstaged.is_empty() && files(app).staged.len() == 2
    });
}

fn shown_diff(t: &mut TestApp) -> Option<(PathBuf, bool)> {
    match t.app().tab().map(|tab| &tab.center) {
        Some(Center::Diff(req)) => Some((
            req.file.path.clone(),
            req.target == gityas_core::diff::DiffTarget::WorkingTree { staged: true },
        )),
        _ => None,
    }
}

#[test]
fn staging_the_shown_file_moves_on_to_the_next_one() {
    let mut t = TestApp::engine();
    let (_dir, _repo) = dirty_repo(&mut t);
    row(&t, "a.txt").click();
    t.harness.step();
    t.harness.step();
    assert_eq!(shown_diff(&mut t), Some((PathBuf::from("a.txt"), false)));

    // The next unstaged file is shown and selected.
    t.harness.get_by_label("Stage file").click();
    t.settle_until(
        "new.txt shown",
        |app| matches!(app.tab().map(|tab| &tab.center), Some(Center::Diff(r)) if r.file.path == Path::new("new.txt")),
    );
    assert_eq!(staged_paths(t.app()), [PathBuf::from("a.txt")]);
    assert_eq!(shown_diff(&mut t), Some((PathBuf::from("new.txt"), false)));
    assert!(wt(t.app()).selection.contains(Section::Unstaged, Path::new("new.txt")));

    // The last unstaged file: the viewer closes and nothing stays selected.
    t.harness.step();
    t.harness.get_by_label("Stage file").click();
    t.settle_until("all staged", |app| files(app).unstaged.is_empty());
    // The command's completion (which closes the viewer) may land after its events.
    t.settle();
    t.harness.step();
    assert_eq!(shown_diff(&mut t), None);
    assert!(wt(t.app()).selection.is_empty());

    // Unstaging from the staged list moves on the same way (the hover button acts on the shown file too).
    row(&t, "new.txt").click();
    t.harness.step();
    t.harness.step();
    assert_eq!(shown_diff(&mut t), Some((PathBuf::from("new.txt"), true)));
    row(&t, "new.txt").hover();
    t.harness.step();
    t.harness.get_by_label("Unstage").click();
    t.settle_until("new.txt unstaged", |app| staged_paths(app) == [PathBuf::from("a.txt")]);
    // The command's completion (which moves the viewer on) may land after its events.
    t.settle();
    t.harness.step();
    assert_eq!(shown_diff(&mut t), Some((PathBuf::from("a.txt"), true)));
    assert!(wt(t.app()).selection.contains(Section::Staged, Path::new("a.txt")));

    // The last staged file: the viewer closes.
    t.harness.get_by_label("Unstage file").click();
    t.settle_until("nothing staged", |app| files(app).staged.is_empty());
    t.settle();
    t.harness.step();
    assert_eq!(shown_diff(&mut t), None);
    assert!(wt(t.app()).selection.is_empty());
}

#[test]
fn closing_the_viewer_deselects_the_file() {
    let mut t = TestApp::engine();
    let (_dir, _repo) = dirty_repo(&mut t);
    row(&t, "a.txt").click();
    t.harness.step();
    t.harness.step();
    assert!(wt(t.app()).selection.contains(Section::Unstaged, Path::new("a.txt")));
    // ✕ and Esc both close the viewer.
    t.harness.key_press(egui::Key::Escape);
    t.harness.step();
    assert_eq!(shown_diff(&mut t), None);
    assert!(wt(t.app()).selection.is_empty());
}

#[test]
fn selection_drives_the_section_button_and_unstage() {
    let mut t = TestApp::engine();
    let (_dir, _repo) = dirty_repo(&mut t);
    t.harness.get_by_label("Stage all changes").click();
    t.settle_until("staged", |app| files(app).staged.len() == 2);

    // Plain click selects and opens the staged diff; the button counts the selection.
    row(&t, "new.txt").click();
    t.harness.step();
    t.harness.step();
    match &t.app().tab().map(|tab| tab.center.clone()) {
        Some(Center::Diff(req)) => {
            assert_eq!(req.file.path, Path::new("new.txt"));
            assert_eq!(req.target, gityas_core::diff::DiffTarget::WorkingTree { staged: true });
        }
        other => panic!("expected a diff, got {other:?}"),
    }
    t.harness.get_by_label("Unstage 1 file").click();
    t.settle_until("new.txt unstaged", |app| staged_paths(app) == [PathBuf::from("a.txt")]);
    // The command's completion (which moves the viewer on) may land after its events.
    t.settle();
    // The unstaged file was on screen: the viewer moved on to the next staged file and selected it.
    t.harness.step();
    let selection = &wt(t.app()).selection;
    assert!(selection.contains(Section::Staged, Path::new("a.txt")) && selection.count_in(Section::Staged) == 1);

    t.harness.get_by_label("Unstage 1 file").click();
    t.settle_until("all unstaged", |app| files(app).staged.is_empty() && files(app).unstaged.len() == 2);
}

#[test]
fn cmd_and_shift_click_select_several_rows() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    t.add_and_open(dir.path());
    for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        write_file(&repo, name, "x\n");
    }
    refresh_files(&mut t);
    t.settle_until("4 files", |app| files(app).unstaged.len() == 4);

    row(&t, "a.txt").click();
    t.harness.step();
    row(&t, "c.txt").click_modifiers(Modifiers::SHIFT);
    t.harness.step();
    t.harness.get_by_label("Stage 3 files");
    row(&t, "b.txt").click_modifiers(Modifiers::COMMAND);
    t.harness.step();
    t.harness.get_by_label("Stage 2 files").click();
    t.settle_until("a and c staged", |app| {
        staged_paths(app) == [PathBuf::from("a.txt"), PathBuf::from("c.txt")]
    });
}

#[test]
fn discard_asks_first_and_handles_tracked_and_untracked_files() {
    let mut t = TestApp::engine();
    let (_dir, repo) = dirty_repo(&mut t);
    let root = workdir(&repo);

    // Select both, right-click → Discard changes… → Cancel: nothing happens.
    row(&t, "a.txt").click();
    t.harness.step();
    row(&t, "new.txt").click_modifiers(Modifiers::COMMAND);
    t.harness.step();
    row(&t, "new.txt").click_secondary();
    t.harness.step();
    menu_click(&mut t, "Discard changes…");
    t.harness.step();
    t.harness.step();
    t.harness
        .get_by_label("Discard changes to 1 file and delete 1 untracked file? This can't be undone.");
    t.harness.get_by_label("Cancel").click();
    t.settle();
    assert!(t.app().confirm.is_none());
    assert!(root.join("new.txt").exists());

    // Right-click on an unselected row selects only it.
    row(&t, "a.txt").click();
    t.harness.step();
    row(&t, "new.txt").click_secondary();
    t.harness.step();
    menu_click(&mut t, "Discard changes…");
    t.harness.step();
    t.harness.step();
    t.harness.get_by_label("Delete untracked file new.txt?");
    t.harness.get_by_label("Delete").click();
    t.settle_until("new.txt deleted", |app| unstaged_paths(app) == [PathBuf::from("a.txt")]);
    assert!(!root.join("new.txt").exists());

    row(&t, "a.txt").click_secondary();
    t.harness.step();
    menu_click(&mut t, "Discard changes…");
    t.harness.step();
    t.harness.step();
    t.harness.get_by_label("Discard changes to a.txt? This can't be undone.");
    t.harness.get_by_label("Discard").click();
    t.settle_until("clean", |app| files(app).is_clean());
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).ok().as_deref(), Some("one\n"));
    t.harness.get_by_label("No unstaged changes");
    t.harness.get_by_label("No staged files");
}

#[test]
fn commit_with_cmd_enter_clears_the_draft_and_toasts() {
    let mut t = TestApp::engine();
    let (_dir, repo) = dirty_repo(&mut t);
    set_signature(&mut t);
    row(&t, "a.txt").hover();
    t.harness.step();
    t.harness.get_by_label("Stage").click();
    t.settle_until("staged", |app| files(app).staged.len() == 1);

    let summary = t.harness.get_by(|n| n.placeholder() == Some("Summary"));
    summary.focus();
    t.harness.step();
    t.harness.get_by(|n| n.placeholder() == Some("Summary")).type_text("Change a");
    t.harness.step();
    assert_eq!(t.app().tab().map(|tab| tab.worktree.summary.as_str()), Some("Change a"));
    wt(t.app()).description = "Longer text\n".into();
    t.harness.get_by_label("Commit changes to 1 file");
    t.harness.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    t.settle_until("committed", |app| {
        app.store.repo.as_ref().is_some_and(|r| r.commits.len() == 3) && files(app).staged.is_empty()
    });
    // The toast and the cleared draft come from the command's completion, which can land after the events.
    t.settle();
    let head = repo.head().and_then(|h| h.peel_to_commit()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(head.message(), Some("Change a\n\nLonger text\n"));
    assert_eq!(head.author().name(), Some("Ada Lovelace"));
    let short = head.id().to_string()[..7].to_string();
    assert!(t.app().notify.toasts().any(|toast| toast.title == format!("Committed {short}")));
    let tab = &t.app().tab().unwrap_or_else(|| panic!("tab")).worktree;
    assert!(tab.summary.is_empty() && tab.description.is_empty());
}

#[test]
fn first_commit_on_an_unborn_head() {
    let mut t = TestApp::engine();
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let repo =
        git2::Repository::init_opts(dir.path(), git2::RepositoryInitOptions::new().initial_head("main")).unwrap_or_else(|e| panic!("{e}"));
    write_file(&repo, "first.txt", "hi\n");
    t.add_and_open(dir.path());
    set_signature(&mut t);
    t.settle_until("file listed", |app| files(app).unstaged.len() == 1);
    assert!(t.harness.query_all_by_label_contains("main (no commits yet)").next().is_some());
    t.harness.get_by_label("Stage all changes").click();
    t.settle_until("staged", |app| files(app).staged.len() == 1);
    wt(t.app()).summary = "Initial commit".into();
    t.harness.step();
    t.harness.get_by_label("Commit changes to 1 file").click();
    t.settle_until("first commit", |app| {
        app.store
            .repo
            .as_ref()
            .is_some_and(|r| r.commits.len() == 1 && r.head.as_ref().is_some_and(|h| !h.is_unborn()))
    });
    let head = repo.head().and_then(|h| h.peel_to_commit()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(head.parent_count(), 0);
    assert_eq!(head.summary(), Some("Initial commit"));
}

#[test]
fn commit_without_signature_asks_for_one_and_does_not_commit() {
    let mut t = TestApp::engine();
    let (_dir, repo) = dirty_repo(&mut t);
    let before = repo.head().ok().and_then(|h| h.target());
    t.harness.get_by_label("Stage all changes").click();
    t.settle_until("staged", |app| files(app).staged.len() == 2);
    wt(t.app()).summary = "No signature".into();
    t.harness.step();
    t.harness.get_by_label("No commit signature set yet; you will be asked for one.");
    t.harness.get_by_label("Commit changes to 2 files").click();
    t.settle();
    // `settings::request_signature` was asked; the settings UI shows its dialog.
    assert!(t.app().worktree_ui.awaiting_signature);
    assert!(!t.app().bridge.is_running(super::TASK_COMMIT));
    assert_eq!(repo.head().ok().and_then(|h| h.target()), before);
    t.harness.get_by_label("Waiting for your commit signature…");
}

#[test]
fn disabled_commit_button_says_why() {
    let mut t = TestApp::engine();
    let (_dir, _repo) = dirty_repo(&mut t);
    // Nothing staged: Cmd+Enter in the summary shows the reason instead of committing.
    t.harness.get_by(|n| n.placeholder() == Some("Summary")).focus();
    t.harness.step();
    wt(t.app()).summary = "x".into();
    t.harness.step();
    t.harness.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    t.settle();
    assert!(t.app().notify.toasts().any(|toast| toast.title == "Stage files to commit"));
}

/// A merge of `feature` into `main` that conflicts on `a.txt`, left in progress (git2 writes MERGE_HEAD / MERGE_MSG).
fn conflicting_merge() -> (tempfile::TempDir, git2::Repository) {
    let (dir, repo) = temp_repo();
    write_file(&repo, "a.txt", "base\n");
    let base = commit_all(&repo, "base");
    {
        let base_commit = repo.find_commit(base).unwrap_or_else(|e| panic!("{e}"));
        repo.branch("feature", &base_commit, false).unwrap_or_else(|e| panic!("{e}"));
    }
    write_file(&repo, "a.txt", "main side\n");
    commit_all(&repo, "main change");
    repo.set_head("refs/heads/feature").unwrap_or_else(|e| panic!("{e}"));
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap_or_else(|e| panic!("{e}"));
    write_file(&repo, "a.txt", "feature side\n");
    commit_all(&repo, "feature change");
    repo.set_head("refs/heads/main").unwrap_or_else(|e| panic!("{e}"));
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap_or_else(|e| panic!("{e}"));
    {
        let feature = repo
            .find_reference("refs/heads/feature")
            .and_then(|r| repo.reference_to_annotated_commit(&r))
            .unwrap_or_else(|e| panic!("{e}"));
        repo.merge(&[&feature], None, None).unwrap_or_else(|e| panic!("merge: {e}"));
    }
    assert!(repo.index().map(|i| i.has_conflicts()).unwrap_or(false));
    (dir, repo)
}

#[test]
fn merge_in_progress_prefills_lists_conflicts_and_commits_the_merge() {
    let mut t = TestApp::engine();
    let (dir, repo) = conflicting_merge();
    t.add_and_open(dir.path());
    set_signature(&mut t);
    t.settle_until("merge status", |app| {
        app.store
            .repo
            .as_ref()
            .is_some_and(|r| r.loaded.status && r.status.operation == RepoOperation::Merge)
    });
    t.harness.step();
    let summary = t.app().tab().map(|tab| tab.worktree.summary.clone()).unwrap_or_default();
    assert!(summary.starts_with("Merge branch 'feature'"), "prefill was {summary:?}");

    t.harness.get_by_label_contains("Conflicted files (1)");
    t.harness.get_by_label("Commit merge");
    // Conflicted rows are not in the Unstaged section; clicking one opens the merge editor.
    t.harness.get_by_label("Unstaged files (0)");
    row(&t, "a.txt").click();
    t.harness.step();
    assert_eq!(
        t.app().tab().map(|tab| tab.center.clone()),
        Some(Center::Merge(PathBuf::from("a.txt")))
    );
    assert_eq!(super::model::section_files(files(t.app()), Section::Conflicted).len(), 1);

    // Resolve outside the app (the merge editor does this via the engine), then commit the merge.
    write_file(&repo, "a.txt", "resolved\n");
    let mut index = repo.index().unwrap_or_else(|e| panic!("{e}"));
    index.add_path(Path::new("a.txt")).unwrap_or_else(|e| panic!("{e}"));
    index.write().unwrap_or_else(|e| panic!("{e}"));
    refresh_files(&mut t);
    t.settle_until("conflict resolved", |app| !files(app).has_conflicts());
    // The editor collapsed the sidebars; closing it brings the commit panel back.
    t.harness.get_by_label("Close merge editor").click();
    for _ in 0..4 {
        t.harness.step();
    }
    t.harness.get_by_label("Commit merge").click();
    t.settle_until("merge committed", |app| {
        app.store.repo.as_ref().is_some_and(|r| r.status.operation == RepoOperation::None)
    });
    let head = repo.head().and_then(|h| h.peel_to_commit()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(head.parent_count(), 2);
    assert!(t.app().tab().is_some_and(|tab| tab.worktree.summary.is_empty()));
}

#[test]
fn stash_all_then_pop() {
    let mut t = TestApp::engine();
    let (_dir, repo) = dirty_repo(&mut t);
    set_signature(&mut t);
    super::open_stash_dialog(t.app(), None);
    t.harness.step();
    t.harness.step();
    t.harness.get_by_label("Stash changes");
    t.harness.get_by_label("Include untracked files");
    t.harness.get_by_label("Stash").click();
    t.settle_until("stashed", |app| {
        files(app).is_clean() && app.store.repo.as_ref().is_some_and(|r| r.stashes.len() == 1)
    });
    // The events can arrive before the command's completion (which closes the dialog) is applied.
    t.settle();
    assert!(t.app().worktree_ui.stash.is_none());
    assert!(!workdir(&repo).join("new.txt").exists());

    super::pop_latest(t.app());
    t.settle_until("popped", |app| {
        files(app).unstaged.len() == 2 && app.store.repo.as_ref().is_some_and(|r| r.stashes.is_empty())
    });
    t.settle();
    assert!(t.app().notify.toasts().any(|toast| toast.title.starts_with("Popped stash@{0}")));

    // Nothing left to pop: info, no command.
    super::pop_latest(t.app());
    t.harness.step();
    t.settle_until("pop again", |app| app.store.repo.as_ref().is_some_and(|r| r.stashes.is_empty()));
}

#[test]
fn stash_selected_from_the_context_menu() {
    let mut t = TestApp::engine();
    let (_dir, repo) = dirty_repo(&mut t);
    set_signature(&mut t);
    row(&t, "a.txt").click_secondary();
    t.harness.step();
    menu_click(&mut t, "Stash selected…");
    t.harness.step();
    t.harness.step();
    t.harness.get_by_label("Stash 1 file");
    t.harness.get_by_label("Stash").click();
    t.settle_until("a.txt stashed", |app| {
        unstaged_paths(app) == [PathBuf::from("new.txt")] && app.store.repo.as_ref().is_some_and(|r| r.stashes.len() == 1)
    });
    assert_eq!(std::fs::read_to_string(workdir(&repo).join("a.txt")).ok().as_deref(), Some("one\n"));
}

#[test]
fn stash_with_only_untracked_files_and_untracked_excluded_is_disabled() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    t.add_and_open(dir.path());
    write_file(&repo, "only-new.txt", "x\n");
    refresh_files(&mut t);
    t.settle_until("listed", |app| files(app).unstaged.len() == 1);
    super::open_stash_dialog(t.app(), None);
    t.harness.step();
    t.harness.step();
    t.harness.get_by_label("Include untracked files").click();
    t.harness.step();
    t.harness.step();
    t.harness.get_by_label("There are no changes to stash.");
    t.harness.get_by_label("Stash").click();
    t.settle();
    assert!(t.app().worktree_ui.stash.is_some());
    assert!(t.app().store.repo.as_ref().is_some_and(|r| r.stashes.is_empty()));
}

#[test]
fn pop_conflict_keeps_the_stash_and_warns() {
    let mut t = TestApp::engine();
    let (_dir, repo) = dirty_repo(&mut t);
    set_signature(&mut t);
    super::open_stash_dialog(t.app(), None);
    t.harness.step();
    t.harness.step();
    t.harness.get_by_label("Stash").click();
    t.settle_until("stashed", |app| app.store.repo.as_ref().is_some_and(|r| r.stashes.len() == 1));
    // Commit a conflicting change to a.txt, then pop.
    write_file(&repo, "a.txt", "three\n");
    commit_all(&repo, "conflicting");
    let engine = t.app().bridge.engine().clone();
    t.block_on(engine.refresh(Refresh::ALL)).unwrap_or_else(|e| panic!("{e}"));
    t.settle();
    super::pop_latest(t.app());
    t.settle_until("pop finished", |app| {
        app.notify
            .toasts()
            .any(|toast| toast.title == "Stash applied with conflicts" || toast.title.starts_with("Could not"))
    });
    assert!(t.app().notify.toasts().any(|toast| toast.title == "Stash applied with conflicts"));
    assert!(t.app().store.repo.as_ref().is_some_and(|r| r.stashes.len() == 1));
}

#[test]
fn tree_toggle_groups_by_directory_and_collapses() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    t.add_and_open(dir.path());
    write_file(&repo, "src/ui/app.rs", "x\n");
    write_file(&repo, "src/lib.rs", "x\n");
    refresh_files(&mut t);
    t.settle_until("listed", |app| files(app).unstaged.len() == 2);
    t.harness.get_by_label_contains("Tree").click();
    t.harness.step();
    assert!(t.app().tab().is_some_and(|tab| tab.worktree.tree));
    t.harness.get_by_label("src/");
    t.harness.get_by_label("ui/").click();
    t.harness.step();
    t.harness.step();
    assert!(t.harness.query_by_label("src/ui/app.rs").is_none());
    row(&t, "src/lib.rs");
}

#[test]
fn mock_mode_smoke() {
    let mut t = TestApp::mock();
    t.harness.step();
    t.harness.get_by_label_contains("5 file changes on");
    t.harness.get_by_label("Unstaged files (4)");
    t.harness.get_by_label("Staged files (1)");
    t.harness.get_by_label("Commit changes to 1 file");
    // Commands fail in mock mode and are reported (no panic, no optimistic change).
    t.harness.get_by_label("Stage all changes").click();
    t.settle();
    assert_eq!(files(t.app()).unstaged.len(), 4);
    assert!(t.app().notify.toasts().any(|toast| toast.title == "Could not stage files"));
    let renamed = files(t.app()).unstaged.iter().find(|d| d.change == DeltaChange::Renamed).cloned();
    assert!(renamed.is_some_and(|d| d.old_path.is_some()));
}

#[test]
#[ignore = "needs a GPU"]
fn snapshot_wip_panel() {
    let mut t = TestApp::mock_gpu();
    t.harness.step();
    t.snapshot("wip_panel");
}

/// After a stash pop with conflicts (no merge in progress), conflicted rows multi-select with Cmd and the section
/// button marks just the selected ones resolved.
#[test]
fn conflicted_files_multi_select_and_mark_resolved() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    for f in ["a.txt", "b.txt", "c.txt"] {
        write_file(&repo, f, "one\n");
    }
    commit_all(&repo, "base");
    t.add_and_open(dir.path());
    set_signature(&mut t);
    for f in ["a.txt", "b.txt", "c.txt"] {
        write_file(&repo, f, "stashed\n");
    }
    refresh_files(&mut t);
    t.settle_until("WIP listed", |app| files(app).unstaged.len() == 3);
    super::open_stash_dialog(t.app(), None);
    t.harness.step();
    t.harness.step();
    t.harness.get_by_label("Stash").click();
    t.settle_until("stashed", |app| app.store.repo.as_ref().is_some_and(|r| r.stashes.len() == 1));
    for f in ["a.txt", "b.txt", "c.txt"] {
        write_file(&repo, f, "committed\n");
    }
    commit_all(&repo, "conflicting");
    let engine = t.app().bridge.engine().clone();
    t.block_on(engine.refresh(Refresh::ALL)).unwrap_or_else(|e| panic!("{e}"));
    t.settle();
    super::pop_latest(t.app());
    t.settle_until("conflicts listed", |app| files(app).conflicted().count() == 3);
    t.harness.step();
    t.harness.get_by_label("Mark all resolved");

    // Cmd+click selects without opening the merge editor.
    row(&t, "a.txt").click_modifiers(Modifiers::COMMAND);
    t.harness.step();
    row(&t, "c.txt").click_modifiers(Modifiers::COMMAND);
    t.harness.step();
    assert_eq!(t.app().tab().map(|tab| tab.center.clone()), Some(Center::Graph));
    assert_eq!(wt(t.app()).selection.count_in(Section::Conflicted), 2);
    t.harness.get_by_label("Mark 2 files resolved").click();
    t.settle_until("two resolved", |app| files(app).conflicted().count() == 1);
    // The toast comes from the command's completion, which may land after its events.
    t.settle();
    let conflicted: Vec<PathBuf> = files(t.app()).conflicted().map(|d| d.path.clone()).collect();
    assert_eq!(conflicted, [PathBuf::from("b.txt")]);
    let mut staged = staged_paths(t.app());
    staged.sort();
    assert_eq!(staged, [PathBuf::from("a.txt"), PathBuf::from("c.txt")]);
    assert!(t.app().notify.toasts().any(|toast| toast.title == "Marked 2 files resolved"));
}

#[test]
fn clicking_the_open_file_again_closes_its_diff() {
    let mut t = TestApp::engine();
    let (_dir, _repo) = dirty_repo(&mut t);
    row(&t, "a.txt").click();
    t.harness.step();
    assert!(matches!(t.app().tab().map(|tab| tab.center.clone()), Some(Center::Diff(_))));
    row(&t, "a.txt").click();
    t.harness.step();
    assert_eq!(t.app().tab().map(|tab| tab.center.clone()), Some(Center::Graph));
    // Another file still switches the diff instead of closing it.
    row(&t, "a.txt").click();
    t.harness.step();
    row(&t, "new.txt").click();
    t.harness.step();
    assert!(matches!(t.app().tab().map(|tab| tab.center.clone()), Some(Center::Diff(r)) if r.file.path == Path::new("new.txt")));
}

#[test]
fn clicking_a_selected_file_again_deselects_it() {
    let mut t = TestApp::engine();
    let (_dir, _repo) = dirty_repo(&mut t);
    row(&t, "a.txt").click();
    t.harness.step();
    t.harness.step();
    t.harness.get_by_label("Stage 1 file");
    row(&t, "a.txt").click();
    t.harness.step();
    t.harness.step();
    assert!(wt(t.app()).selection.is_empty());
    assert_eq!(
        t.app().tab().map(|tab| tab.center.clone()),
        Some(Center::Graph),
        "its diff closes too"
    );
    t.harness.get_by_label("Stage all changes");
}
