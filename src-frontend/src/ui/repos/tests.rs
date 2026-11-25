//! Headless UI tests of the repositories module against the real (in-memory) engine and temp repositories, plus a
//! mock-mode smoke test of the picker. Pure logic is tested in `logic.rs`.

use std::path::Path;

use egui::PointerButton;
use egui_kittest::kittest::Queryable;
use gityas_core::models::{OpenTabs, Repository};
use gityas_core::{Provider, RepoId};

use super::*;
use crate::testutil::{TestApp, temp_repo};

fn open_tabs(t: &TestApp) -> OpenTabs {
    let engine = t.harness.state().bridge.engine().clone();
    t.block_on(engine.open_tabs()).unwrap_or_else(|e| panic!("open_tabs: {e}"))
}

fn ids(repos: &[&Repository]) -> Vec<RepoId> {
    repos.iter().map(|r| r.id.clone()).collect()
}

/// Waits until no session operation / tab write is pending and the store shows `active`.
fn settle_on(t: &mut TestApp, active: Option<&RepoId>) {
    let want = active.cloned();
    t.settle_until("session settled", |app| {
        app.repos_ui.queue.is_empty()
            && app.repos_ui.opening.is_none()
            && !app.bridge.is_running(TASK_SESSION)
            && !app.bridge.is_running(TASK_PERSIST_TABS)
            && app.store.active == want
            && want
                .as_ref()
                .is_none_or(|id| app.store.repo.as_ref().is_some_and(|r| &r.id == id && r.is_ready()))
    });
    t.settle();
}

/// Registers `path` without opening it (as if added earlier).
fn register(t: &mut TestApp, path: &Path) -> Repository {
    let engine = t.harness.state().bridge.engine().clone();
    let repo = t
        .block_on(engine.add_repository(path.to_path_buf(), None))
        .unwrap_or_else(|e| panic!("add_repository: {e}"));
    let id = repo.id.clone();
    t.settle_until("registered", |app| app.store.repository(&id).is_some());
    repo
}

/// Clicks the dialog's primary button (its label repeats the dialog title / the tab name; it is drawn last).
fn primary(t: &mut TestApp, label: &str) {
    // The modal fades in; it doesn't take clicks on its first frames.
    for _ in 0..10 {
        t.harness.step();
    }
    t.harness
        .get_all_by_label(label)
        .last()
        .unwrap_or_else(|| panic!("no {label} button"))
        .click();
}

fn add_dialog(t: &mut TestApp) -> &mut add_dialog::AddDialog {
    t.app().repos_ui.add.as_mut().unwrap_or_else(|| panic!("add dialog closed"))
}

// ---- Tabs -------------------------------------------------------------------------------------------------------

#[test]
fn open_two_repositories_and_switch_between_tabs() {
    let mut t = TestApp::engine();
    let (dir_a, _a) = temp_repo();
    let (dir_b, _b) = temp_repo();
    let a = t.add_and_open(dir_a.path());
    let b = t.add_and_open(dir_b.path());
    settle_on(&mut t, Some(&b.id));
    assert_eq!(t.app().store.open_tabs.open, ids(&[&a, &b]));

    t.harness.get_by_label(&a.name).click();
    settle_on(&mut t, Some(&a.id));
    assert_eq!(
        open_tabs(&t),
        OpenTabs {
            open: ids(&[&a, &b]),
            active: Some(a.id.clone())
        }
    );

    // Ctrl+Tab / Cmd+2 equivalents.
    switch_relative(t.app(), 1);
    settle_on(&mut t, Some(&b.id));
    switch_to_index(t.app(), 0);
    settle_on(&mut t, Some(&a.id));
    switch_to_index(t.app(), 7); // no such tab
    settle_on(&mut t, Some(&a.id));
    assert_eq!(open_tabs(&t).active.as_ref(), Some(&a.id));
}

#[test]
fn rapid_switches_end_on_the_last_one_clicked() {
    let mut t = TestApp::engine();
    let (dir_a, _a) = temp_repo();
    let (dir_b, _b) = temp_repo();
    let (dir_c, _c) = temp_repo();
    let a = t.add_and_open(dir_a.path());
    let b = t.add_and_open(dir_b.path());
    let c = t.add_and_open(dir_c.path());
    settle_on(&mut t, Some(&c.id));
    // Three switches in one frame: the queue keeps only the newest pending one.
    switch_to(t.app(), a.id.clone());
    switch_to(t.app(), b.id.clone());
    switch_to(t.app(), a.id.clone());
    assert!(t.app().repos_ui.queue.len() <= 1);
    settle_on(&mut t, Some(&a.id));
    assert_eq!(t.app().bridge.engine().active_repository().as_ref(), Some(&a.id));
    assert_eq!(
        open_tabs(&t),
        OpenTabs {
            open: ids(&[&a, &b, &c]),
            active: Some(a.id.clone())
        }
    );
}

#[test]
fn closing_an_inactive_tab_keeps_the_session() {
    let mut t = TestApp::engine();
    let (dir_a, _a) = temp_repo();
    let (dir_b, _b) = temp_repo();
    let a = t.add_and_open(dir_a.path());
    let b = t.add_and_open(dir_b.path());
    settle_on(&mut t, Some(&b.id));

    t.harness.get_by_label(&format!("Close {}", a.name)).click();
    settle_on(&mut t, Some(&b.id));
    assert_eq!(t.app().store.open_tabs.open, ids(&[&b]));
    assert_eq!(
        open_tabs(&t),
        OpenTabs {
            open: ids(&[&b]),
            active: Some(b.id.clone())
        }
    );
    assert!(t.app().store.repository(&a.id).is_some(), "the registration stays");
}

#[test]
fn closing_the_active_tab_opens_the_neighbour_then_the_welcome_screen() {
    let mut t = TestApp::engine();
    let (dir_a, _a) = temp_repo();
    let (dir_b, _b) = temp_repo();
    let a = t.add_and_open(dir_a.path());
    let b = t.add_and_open(dir_b.path());
    t.harness.get_by_label(&a.name).click();
    settle_on(&mut t, Some(&a.id));

    // Middle-click closes too.
    t.harness.get_by_label(&a.name).click_button(PointerButton::Middle);
    settle_on(&mut t, Some(&b.id));
    assert_eq!(
        open_tabs(&t),
        OpenTabs {
            open: ids(&[&b]),
            active: Some(b.id.clone())
        }
    );

    close_active_tab(t.app()); // Cmd+W
    settle_on(&mut t, None);
    assert!(t.app().store.repo.is_none() && t.app().store.open_tabs.open.is_empty());
    assert_eq!(open_tabs(&t), OpenTabs::default());
    t.harness.get_by_label("No repository open");
    // Both stay registered and are offered as recent repositories.
    t.harness.get_by_label(&a.name).click();
    settle_on(&mut t, Some(&a.id));
    assert_eq!(
        open_tabs(&t),
        OpenTabs {
            open: ids(&[&a]),
            active: Some(a.id.clone())
        }
    );
}

#[test]
fn close_others_from_the_tab_context_menu() {
    let mut t = TestApp::engine();
    let (dir_a, _a) = temp_repo();
    let (dir_b, _b) = temp_repo();
    let (dir_c, _c) = temp_repo();
    let a = t.add_and_open(dir_a.path());
    let b = t.add_and_open(dir_b.path());
    let c = t.add_and_open(dir_c.path());
    settle_on(&mut t, Some(&c.id));

    t.harness.get_by_label(&b.name).click_secondary();
    t.harness.step();
    t.harness.get_by_label("Close others").click();
    settle_on(&mut t, Some(&b.id));
    assert_eq!(
        open_tabs(&t),
        OpenTabs {
            open: ids(&[&b]),
            active: Some(b.id.clone())
        }
    );
    assert!(t.app().store.repository(&a.id).is_some());
}

#[test]
fn reordering_tabs_is_persisted() {
    let mut t = TestApp::engine();
    let (dir_a, _a) = temp_repo();
    let (dir_b, _b) = temp_repo();
    let (dir_c, _c) = temp_repo();
    let a = t.add_and_open(dir_a.path());
    let b = t.add_and_open(dir_b.path());
    let c = t.add_and_open(dir_c.path());
    settle_on(&mut t, Some(&c.id));

    // What a drop of `a` onto the right half of `c` does.
    move_tab(t.app(), 0, logic::drop_index(0, 2, true));
    settle_on(&mut t, Some(&c.id));
    assert_eq!(
        open_tabs(&t),
        OpenTabs {
            open: ids(&[&b, &c, &a]),
            active: Some(c.id.clone())
        }
    );
}

#[test]
fn removing_the_active_repository_asks_first_and_opens_the_neighbour() {
    let mut t = TestApp::engine();
    let (dir_a, _a) = temp_repo();
    let (dir_b, _b) = temp_repo();
    let a = t.add_and_open(dir_a.path());
    let b = t.add_and_open(dir_b.path());
    settle_on(&mut t, Some(&b.id));

    t.harness.get_by_label(&b.name).click_secondary();
    t.harness.step();
    t.harness.get_by_label("Remove from GitYas…").click();
    t.harness.step();
    t.harness.step();
    let confirm = t.app().confirm.as_ref().unwrap_or_else(|| panic!("no confirmation"));
    assert_eq!(
        confirm.body,
        format!("Remove {} from GitYas? The folder on disk is not deleted.", b.name)
    );

    // Cancel: nothing happens.
    t.harness.get_by_label("Cancel").click();
    t.harness.step();
    assert!(t.app().confirm.is_none());
    assert!(t.app().store.repository(&b.id).is_some());

    confirm_remove(t.app(), b.id.clone());
    t.harness.step();
    t.harness.get_by_label("Remove repository").click();
    settle_on(&mut t, Some(&a.id));
    assert!(t.app().store.repository(&b.id).is_none());
    assert!(dir_b.path().join("README.md").exists(), "files are never deleted");
    assert_eq!(
        open_tabs(&t),
        OpenTabs {
            open: ids(&[&a]),
            active: Some(a.id.clone())
        }
    );
    assert!(
        t.app()
            .notify
            .toasts()
            .any(|toast| toast.title == format!("Removed {} from GitYas", b.name))
    );
}

#[test]
fn removing_an_inactive_or_the_last_repository() {
    let mut t = TestApp::engine();
    let (dir_a, _a) = temp_repo();
    let (dir_b, _b) = temp_repo();
    let a = t.add_and_open(dir_a.path());
    let b = register(&mut t, dir_b.path());
    settle_on(&mut t, Some(&a.id));

    remove_repository(t.app(), b.id.clone(), b.name.clone());
    settle_on(&mut t, Some(&a.id));
    assert!(t.app().store.repository(&b.id).is_none());

    remove_repository(t.app(), a.id.clone(), a.name.clone());
    settle_on(&mut t, None);
    assert!(t.app().store.repositories.is_empty() && t.app().store.repo.is_none());
    assert_eq!(open_tabs(&t), OpenTabs::default());
}

// ---- Picker -----------------------------------------------------------------------------------------------------

#[test]
fn picker_opens_repositories_and_reorders_them() {
    let mut t = TestApp::engine();
    let (dir_a, _a) = temp_repo();
    let (dir_b, _b) = temp_repo();
    let a = register(&mut t, dir_a.path());
    let b = register(&mut t, dir_b.path());

    t.harness.get_by_label("Open a repository").click(); // the tab bar's `+`
    t.harness.step();
    assert!(t.app().repos_ui.picker.is_some());
    // The welcome screen lists them too; the picker's row is drawn last.
    let rows = t.harness.get_all_by_label(&b.name).count();
    assert!(rows >= 2);
    t.harness.get_all_by_label(&b.name).last().unwrap_or_else(|| panic!("row")).click();
    settle_on(&mut t, Some(&b.id));
    assert!(t.app().repos_ui.picker.is_none(), "opening closes the popup");

    // Drag `a` below `b` (what the drop handler computes).
    reorder_repository(t.app(), 0, logic::drop_index(0, 1, true));
    t.settle_until("reordered", |app| app.store.repositories.first().is_some_and(|r| r.id == b.id));
    assert_eq!(
        t.app().store.repositories.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
        ids(&[&b, &a])
    );

    set_provider(t.app(), a.id.clone(), Some(Provider::Gitlab));
    t.settle_until("provider changed", |app| {
        app.store.repository(&a.id).is_some_and(|r| r.provider == Some(Provider::Gitlab))
    });
}

#[test]
fn picker_closes_on_escape_and_shows_the_empty_state() {
    let mut t = TestApp::engine();
    open_picker(t.app());
    t.harness.step();
    t.harness.get_by_label("No repositories yet");
    t.harness.key_press(egui::Key::Escape);
    t.harness.step();
    assert!(t.app().repos_ui.picker.is_none());
}

#[test]
fn mock_picker_smoke_test() {
    let mut t = TestApp::mock();
    t.harness.step();
    t.harness.get_by_label("Open a repository").click();
    t.harness.step();
    t.harness.step();
    assert!(t.app().repos_ui.picker.is_some());
    t.harness.get_by_label("Repositories");
    t.harness.get_by_label("Actions for git-yas");
    if let Some(picker) = t.app().repos_ui.picker.as_mut() {
        picker.filter = "nothing-matches".into();
    }
    t.harness.step();
    t.harness.get_by_label("No repository matches the filter");
    // `Open` in the popup header switches to the add dialog.
    t.harness.get_by_label(&format!("{} Open", egui_phosphor::regular::PLUS)).click();
    t.harness.step();
    t.harness.step();
    assert!(t.app().repos_ui.picker.is_none());
    assert!(t.app().repos_ui.add.as_ref().is_some_and(|d| d.tab == AddTab::Local));
}

// ---- Add local ----------------------------------------------------------------------------------------------------

#[test]
fn add_local_repository_opens_it_in_a_new_tab() {
    let mut t = TestApp::engine();
    let (dir, _repo) = temp_repo();
    open_add_dialog(t.app(), AddTab::Local);
    add_dialog(&mut t).local.folder = dir.path().display().to_string();
    t.harness.step();
    primary(&mut t, "Add repository");
    t.settle_until("added and opened", |app| app.repos_ui.add.is_none() && app.store.repo.is_some());
    let id = t.app().store.repositories[0].id.clone();
    settle_on(&mut t, Some(&id));
    assert_eq!(
        open_tabs(&t),
        OpenTabs {
            open: vec![id.clone()],
            active: Some(id)
        }
    );
    assert!(t.app().notify.toasts().any(|toast| toast.title.starts_with("Added ")));
}

#[test]
fn add_local_reports_already_added_and_not_a_repository_inline() {
    let mut t = TestApp::engine();
    let (dir, _repo) = temp_repo();
    register(&mut t, dir.path());

    // The same folder: caught before asking the engine.
    open_add_dialog(t.app(), AddTab::Local);
    add_dialog(&mut t).local.folder = dir.path().display().to_string();
    add_dialog(&mut t).local.touched = true;
    t.harness.step();
    t.harness.get_by_label("Already added");

    // A subfolder resolves to the same root: the engine says AlreadyExists.
    std::fs::create_dir(dir.path().join("sub")).unwrap_or_else(|e| panic!("{e}"));
    add_dialog(&mut t).local.folder = dir.path().join("sub").display().to_string();
    t.harness.step();
    primary(&mut t, "Add repository");
    t.settle_until("engine answered", |app| {
        app.repos_ui.add.as_ref().is_some_and(|d| d.local.field_error.is_some())
    });
    t.harness.step();
    t.harness.get_by_label("Already added");

    // A plain folder.
    let plain = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    add_dialog(&mut t).local.folder = plain.path().display().to_string();
    add_dialog(&mut t).local.field_error = None;
    t.harness.step();
    primary(&mut t, "Add repository");
    t.settle_until("engine answered", |app| {
        app.repos_ui.add.as_ref().is_some_and(|d| d.local.field_error.is_some())
    });
    t.harness.step();
    t.harness.get_by_label("Not a git repository");
    assert_eq!(t.app().store.repositories.len(), 1);
    assert!(t.app().notify.toasts().next().is_none(), "inline errors, not toasts");
}

// ---- Clone ------------------------------------------------------------------------------------------------------

/// A bare copy of a fresh temp repository: `(holder dir, bare path)`.
fn bare_remote() -> (tempfile::TempDir, std::path::PathBuf, tempfile::TempDir) {
    let (src, _repo) = temp_repo();
    let holder = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let bare = holder.path().join("remote.git");
    git2::build::RepoBuilder::new()
        .bare(true)
        .clone(&src.path().display().to_string(), &bare)
        .unwrap_or_else(|e| panic!("bare clone: {e}"));
    (holder, bare, src)
}

#[test]
fn clone_from_a_local_bare_repository() {
    let mut t = TestApp::engine();
    let (_holder, bare, _src) = bare_remote();
    let into = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));

    open_add_dialog(t.app(), AddTab::Clone);
    add_dialog(&mut t).clone.url = bare.display().to_string();
    add_dialog(&mut t).clone.into = into.path().display().to_string();
    t.harness.step();
    t.harness.get_by_label_contains("Will clone into");
    primary(&mut t, "Clone");
    t.settle_until("cloned and opened", |app| app.repos_ui.add.is_none() && app.store.repo.is_some());
    let repo = t.app().store.repositories[0].clone();
    settle_on(&mut t, Some(&repo.id));
    assert_eq!(repo.name, "remote");
    assert!(into.path().join("remote").join("README.md").exists());
    assert!(t.app().notify.toasts().any(|toast| toast.title.starts_with("Cloned ")));
    assert!(t.app().store.progress.is_none());
}

#[test]
fn clone_validation_blocks_bad_input() {
    let mut t = TestApp::engine();
    let into = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    std::fs::create_dir(into.path().join("repo")).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(into.path().join("repo").join("f"), "x").unwrap_or_else(|e| panic!("{e}"));

    open_add_dialog(t.app(), AddTab::Clone);
    add_dialog(&mut t).clone.into = into.path().display().to_string();
    for (url, message) in [
        ("http://github.com/owner/repo", "Enter an https URL"),
        ("https://github.com/owner/repo.git", "Folder already exists and isn't empty"),
    ] {
        add_dialog(&mut t).clone.url = url.into();
        t.harness.step();
        t.harness.get_by_label(message);
        primary(&mut t, "Clone");
        t.harness.step();
        assert!(!t.app().bridge.is_running("repos.clone"), "{url}: the Clone button is disabled");
    }
    // github.com is detected and no token is saved in the in-memory engine.
    assert!(t.harness.query_by_label("GitHub (detected)").is_some() || t.harness.query_by_value("GitHub (detected)").is_some());
    t.harness.get_by_label("No GitHub token saved —");
    // A name override moves the destination out of the way.
    add_dialog(&mut t).clone.name = "other".into();
    t.harness.step();
    t.harness.get_by_label_contains("Will clone into");
}

#[test]
fn failed_clone_keeps_the_dialog_open_with_the_error() {
    let mut t = TestApp::engine();
    let into = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let missing = into.path().join("does-not-exist.git");
    open_add_dialog(t.app(), AddTab::Clone);
    add_dialog(&mut t).clone.url = missing.display().to_string();
    add_dialog(&mut t).clone.into = into.path().display().to_string();
    t.harness.step();
    primary(&mut t, "Clone");
    t.settle_until("clone failed", |app| {
        app.repos_ui
            .add
            .as_ref()
            .is_some_and(|d| !d.clone.cloning && d.clone.error.is_some())
    });
    assert!(t.app().store.repositories.is_empty());
    assert!(!into.path().join("does-not-exist").exists(), "the partial folder is removed");
}

// ---- Snapshots ----------------------------------------------------------------------------------------------------

#[test]
#[ignore = "needs a GPU"]
fn snapshot_picker_and_add_dialog() {
    let mut t = TestApp::mock_gpu();
    t.harness.step();
    open_picker(t.app());
    for _ in 0..3 {
        t.harness.step();
    }
    t.snapshot("repo_picker");
    open_add_dialog(t.app(), AddTab::Clone);
    add_dialog(&mut t).clone.url = "https://github.com/owner/repo.git".into();
    for _ in 0..10 {
        t.harness.step();
    }
    t.snapshot("add_repository_clone");
}

#[test]
fn overflowing_tabs_keep_their_width_and_the_active_one_scrolls_into_view() {
    let mut t = TestApp::engine();
    let mut dirs = Vec::new();
    let mut names = Vec::new();
    for _ in 0..10 {
        let (dir, _repo) = temp_repo();
        names.push(t.add_and_open(dir.path()).name);
        dirs.push(dir);
    }
    for _ in 0..3 {
        t.harness.step();
    }
    let plus = t.harness.get_by_label("Open a repository").rect();
    let tab = |t: &TestApp, name: &str| t.harness.get_by_label(name).rect();
    // Ten 180 px tabs don't fit in 1440 px: they keep their width and overflow.
    for name in &names {
        let rect = tab(&t, name);
        assert!((rect.width() - 180.0).abs() < 1.0, "{name}: {rect:?}");
    }
    // The last opened (active) tab was scrolled into view, so the first is scrolled out under the `+`.
    let last = tab(&t, &names[9]);
    assert!(last.left() >= plus.right() && last.right() <= 1440.0 + 0.5, "{last:?}");
    assert!(tab(&t, &names[0]).left() < plus.right(), "{:?}", tab(&t, &names[0]));
    // Switching back to the first scrolls it in again.
    let first = t.app().store.open_tabs.open[0].clone();
    switch_to(t.app(), first);
    t.settle_until("switched", |app| {
        app.store.repo.as_ref().is_some_and(|r| r.is_ready()) && opening_label(app).is_none()
    });
    for _ in 0..3 {
        t.harness.step();
    }
    assert!(tab(&t, &names[0]).left() >= plus.right(), "{:?}", tab(&t, &names[0]));
}

#[test]
#[ignore = "needs a GPU"]
fn snapshot_overflowing_tabs() {
    let mut t = TestApp::mock_gpu();
    for i in 0..9 {
        t.app().store.open_tabs.open.push(format!("repository-{i}"));
    }
    for _ in 0..3 {
        t.harness.step();
    }
    t.snapshot("tab_overflow");
}

#[test]
fn a_chosen_folder_is_remembered_for_the_next_dialog() {
    let mut t = TestApp::engine();
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    super::open_add_dialog(t.app(), AddTab::Clone);
    add_dialog(&mut t).picked = Some(dir.path().to_path_buf());
    t.harness.step();
    t.settle_until("last folder saved", |app| !app.bridge.is_running("repos.last_folder"));
    let engine = t.app().bridge.engine().clone();
    let saved = t.block_on(engine.settings()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(saved.last_folder.as_deref(), Some(dir.path().display().to_string().as_str()));

    t.app().repos_ui.add = None;
    super::open_add_dialog(t.app(), AddTab::Clone);
    assert_eq!(add_dialog(&mut t).clone.into, dir.path().display().to_string());
}
