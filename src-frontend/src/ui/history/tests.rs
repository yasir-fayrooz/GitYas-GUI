//! Headless UI tests for the history screens: the real app on an in-memory engine against temp repositories
//! (git2 fixtures), plus a mock-mode smoke test. FSEvents does not work in the sandbox, so fixtures changed with git2
//! are followed by `engine.refresh(..)`.

use egui_kittest::kittest::Queryable;
use gityas_core::diff::DiffTarget;
use gityas_core::models::Head;
use gityas_engine::Refresh;

use super::{Intent, RemoteTags, run};
use crate::app::{Center, Selection, SidePage};
use crate::testutil::{TestApp, commit_all, temp_repo, write_file};

fn refresh(t: &mut TestApp, what: Refresh) {
    let engine = t.app().bridge.engine().clone();
    t.block_on(engine.refresh(what)).unwrap_or_else(|e| panic!("refresh: {e}"));
    t.settle();
}

fn head_name(t: &mut TestApp) -> Option<String> {
    t.app()
        .store
        .repo
        .as_ref()
        .and_then(|r| r.head.as_ref())
        .and_then(Head::branch_name)
        .map(str::to_owned)
}

fn has_local(t: &mut TestApp, name: &str) -> bool {
    t.app()
        .store
        .repo
        .as_ref()
        .is_some_and(|r| r.local_branches.iter().any(|b| b.name == name))
}

fn sig() -> git2::Signature<'static> {
    git2::Signature::now("Test", "test@example.com").unwrap_or_else(|e| panic!("{e}"))
}

/// A commit on `refs/heads/<branch>` (created from HEAD's commit) that HEAD does not contain.
fn commit_on_branch(repo: &git2::Repository, branch: &str, file: &str) -> git2::Oid {
    let head = repo.head().and_then(|h| h.peel_to_commit()).unwrap_or_else(|e| panic!("{e}"));
    let mut tree = repo
        .treebuilder(Some(&head.tree().unwrap_or_else(|e| panic!("{e}"))))
        .unwrap_or_else(|e| panic!("{e}"));
    let blob = repo.blob(b"branch content\n").unwrap_or_else(|e| panic!("{e}"));
    tree.insert(file, blob, 0o100644).unwrap_or_else(|e| panic!("{e}"));
    let tree = repo
        .find_tree(tree.write().unwrap_or_else(|e| panic!("{e}")))
        .unwrap_or_else(|e| panic!("{e}"));
    repo.commit(
        Some(&format!("refs/heads/{branch}")),
        &sig(),
        &sig(),
        "branch commit",
        &tree,
        &[&head],
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

/// Double click. The harness runs one 0.25 s frame per input event, so a click takes longer than egui's
/// double-click delay; widen it for the test.
fn click_twice(t: &mut TestApp, label: &str) {
    branches(t);
    t.harness.ctx.options_mut(|o| o.input_options.max_double_click_delay = 10.0);
    t.harness.get_by_label(label).click();
    t.harness.step();
    t.harness.get_by_label(label).click();
    t.harness.step();
}

/// The button labelled `label` (dialog titles repeat their primary button's text).
fn button(t: &mut TestApp, label: &str) {
    t.harness.step();
    t.harness.get_by_role_and_label(egui::accesskit::Role::Button, label).click();
}

/// Shows the Branches page (the refs sidebar); file changes and selections switch to the Changes page.
fn branches(t: &mut TestApp) {
    t.app().layout.show(SidePage::Branches);
    t.harness.step();
}

/// Opens the context menu of the node labelled `target` (graph or Branches page), then clicks `item`.
fn context_click(t: &mut TestApp, target: &str, item: &str) {
    branches(t);
    t.harness.get_by_label(target).click_secondary();
    t.harness.step();
    t.harness.step();
    t.harness.get_by_label(item).click();
    t.harness.step();
}

#[test]
fn double_click_in_the_sidebar_checks_out_a_branch() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    let main = repo.head().and_then(|h| h.peel_to_commit()).unwrap_or_else(|e| panic!("{e}"));
    repo.branch("feature", &main, false).unwrap_or_else(|e| panic!("{e}"));
    t.add_and_open(dir.path());
    t.settle_until("branches listed", |app| {
        app.store.repo.as_ref().is_some_and(|r| r.local_branches.len() == 2)
    });
    t.harness.step();
    click_twice(&mut t, "feature");
    t.settle_until("feature checked out", |app| {
        app.store
            .repo
            .as_ref()
            .and_then(|r| r.head.as_ref())
            .is_some_and(|h| h.branch_name() == Some("feature"))
    });
    t.settle(); // the completion (toast) may run a frame after the head event
    assert!(t.app().notify.toasts().any(|toast| toast.title == "Checked out feature"));
}

#[test]
fn checkout_refused_by_local_changes_offers_to_stash() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    commit_on_branch(&repo, "other", "README.md");
    write_file(&repo, "README.md", "local edit\n");
    t.add_and_open(dir.path());
    run(
        t.app(),
        vec![Intent::CheckoutBranch {
            refname: "refs/heads/other".into(),
            label: "other".into(),
        }],
    );
    t.settle();
    assert_eq!(head_name(&mut t).as_deref(), Some("main"));
    let action = t
        .app()
        .notify
        .toasts()
        .find(|toast| toast.title == "Could not check out other")
        .and_then(|toast| toast.action.as_ref().map(|a| a.label.clone()));
    assert_eq!(action.as_deref(), Some("Stash changes"));
}

#[test]
fn remote_branch_checkout_creates_a_tracking_branch() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    let remote_dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    git2::Repository::init_bare(remote_dir.path()).unwrap_or_else(|e| panic!("{e}"));
    repo.remote("origin", &remote_dir.path().to_string_lossy())
        .unwrap_or_else(|e| panic!("{e}"));
    let tip = commit_on_branch(&repo, "scratch", "remote.txt");
    repo.reference("refs/remotes/origin/topic", tip, true, "fixture")
        .unwrap_or_else(|e| panic!("{e}"));
    t.add_and_open(dir.path());
    t.settle_until("remote branch listed", |app| {
        app.store
            .repo
            .as_ref()
            .is_some_and(|r| r.remote_branches.iter().any(|b| b.name == "origin/topic"))
    });
    t.harness.step();
    click_twice(&mut t, "origin/topic");
    t.settle_until("tracking branch checked out", |app| {
        app.store.repo.as_ref().is_some_and(|r| {
            r.local_branches
                .iter()
                .any(|b| b.name == "topic" && b.is_head && b.upstream.as_deref() == Some("origin/topic"))
        })
    });
}

#[test]
fn create_branch_dialog_validates_and_creates() {
    let mut t = TestApp::engine();
    let (dir, _repo) = temp_repo();
    t.add_and_open(dir.path());
    super::open_create_branch(t.app(), None);
    t.harness.step();
    let dialog = t.app().history_ui.create_branch.as_mut().unwrap_or_else(|| panic!("dialog"));
    assert_eq!(dialog.from_label, "HEAD (main)");
    dialog.name = "main".into();
    t.harness.step();
    t.harness.get_by_label("Already exists");
    if let Some(d) = t.app().history_ui.create_branch.as_mut() {
        d.name = "bad name".into();
    }
    t.harness.step();
    t.harness.get_by_label("Names can't contain spaces");
    // The disabled button does nothing.
    button(&mut t, "Create branch");
    t.harness.step();
    assert!(!t.app().bridge.is_running(super::actions::TASK_CREATE_BRANCH));

    if let Some(d) = t.app().history_ui.create_branch.as_mut() {
        d.name = "feature/x".into();
    }
    t.harness.step();
    button(&mut t, "Create branch");
    t.settle_until("branch created and checked out", |app| {
        app.history_ui.create_branch.is_none()
            && app
                .store
                .repo
                .as_ref()
                .is_some_and(|r| r.local_branches.iter().any(|b| b.name == "feature/x" && b.is_head))
    });
    // The sidebar shows it in a folder.
    branches(&mut t);
    t.harness.get_by_label("feature/");
}

#[test]
fn create_branch_here_from_the_commit_menu_without_checkout() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    let first = repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .map(|c| c.id())
        .unwrap_or_else(|e| panic!("{e}"));
    write_file(&repo, "b.txt", "b\n");
    commit_all(&repo, "second commit");
    t.add_and_open(dir.path());
    t.harness.step();
    context_click(&mut t, "initial commit", "Create branch here…");
    let dialog = t.app().history_ui.create_branch.as_mut().unwrap_or_else(|| panic!("dialog"));
    assert!(dialog.from_label.ends_with("initial commit"), "{}", dialog.from_label);
    dialog.name = "old".into();
    dialog.checkout = false;
    t.harness.step();
    button(&mut t, "Create branch");
    t.settle_until("branch created", |app| {
        app.store
            .repo
            .as_ref()
            .is_some_and(|r| r.local_branches.iter().any(|b| b.name == "old"))
    });
    let repo_data = t.app().store.repo.as_ref().unwrap_or_else(|| panic!("repo"));
    let old = repo_data.local_branches.iter().find(|b| b.name == "old");
    assert_eq!(old.and_then(|b| b.target.as_ref()).map(|o| o.to_hex()), Some(first.to_string()));
    assert_eq!(head_name(&mut t).as_deref(), Some("main"));
}

#[test]
fn deleting_an_unmerged_branch_asks_again_with_force_delete() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    commit_on_branch(&repo, "topic", "topic.txt");
    let main = repo.head().and_then(|h| h.peel_to_commit()).unwrap_or_else(|e| panic!("{e}"));
    repo.branch("merged", &main, false).unwrap_or_else(|e| panic!("{e}"));
    t.add_and_open(dir.path());
    t.settle_until("branches", |app| {
        app.store.repo.as_ref().is_some_and(|r| r.local_branches.len() == 3)
    });
    t.harness.step();

    // Merged: one confirmation.
    context_click(&mut t, "merged", "Delete merged…");
    assert!(t.app().confirm.as_ref().is_some_and(|c| c.body == "Delete branch merged?"));
    button(&mut t, "Delete branch");
    t.settle_until("merged deleted", |app| {
        app.store.repo.as_ref().is_some_and(|r| r.local_branches.len() == 2)
    });

    // Unmerged: refused, then "Force delete" with the warning.
    t.harness.step();
    context_click(&mut t, "topic", "Delete topic…");
    button(&mut t, "Delete branch");
    t.settle_until("force confirmation", |app| {
        app.confirm.as_ref().is_some_and(|c| c.action == "Force delete")
    });
    let warning = t.app().confirm.as_ref().and_then(|c| c.warning.clone());
    assert_eq!(warning.as_deref(), Some("It isn't merged; its commits will be lost"));
    assert!(has_local(&mut t, "topic"));
    t.harness.step();
    button(&mut t, "Force delete");
    t.settle_until("topic deleted", |app| {
        app.store
            .repo
            .as_ref()
            .is_some_and(|r| r.local_branches.iter().all(|b| b.name != "topic"))
    });
}

#[test]
fn renaming_the_checked_out_branch_from_its_menu() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    let main = repo.head().and_then(|h| h.peel_to_commit()).unwrap_or_else(|e| panic!("{e}"));
    repo.branch("other", &main, false).unwrap_or_else(|e| panic!("{e}"));
    t.add_and_open(dir.path());
    t.settle_until("branches", |app| {
        app.store.repo.as_ref().is_some_and(|r| r.local_branches.len() == 2)
    });
    t.harness.step();

    context_click(&mut t, "main", "Rename main…");
    let dialog = t.app().history_ui.rename_branch.as_mut().unwrap_or_else(|| panic!("dialog"));
    assert_eq!(dialog.name, "main");
    // Unchanged name: the button is disabled without a message.
    button(&mut t, "Rename");
    t.harness.step();
    assert!(!t.app().bridge.is_running(super::actions::TASK_RENAME_BRANCH));
    if let Some(d) = t.app().history_ui.rename_branch.as_mut() {
        d.name = "other".into();
    }
    t.harness.step();
    t.harness.get_by_label("Already exists");

    if let Some(d) = t.app().history_ui.rename_branch.as_mut() {
        d.name = "trunk".into();
    }
    t.harness.step();
    button(&mut t, "Rename");
    t.settle_until("branch renamed, still checked out", |app| {
        app.history_ui.rename_branch.is_none()
            && app.store.repo.as_ref().is_some_and(|r| {
                r.local_branches.iter().any(|b| b.name == "trunk" && b.is_head) && r.local_branches.iter().all(|b| b.name != "main")
            })
    });
    assert_eq!(head_name(&mut t).as_deref(), Some("trunk"));
}

#[test]
fn head_branch_cannot_be_deleted_from_its_menu() {
    let mut t = TestApp::engine();
    let (dir, _repo) = temp_repo();
    t.add_and_open(dir.path());
    branches(&mut t);
    t.harness.get_by_label("main").click_secondary();
    t.harness.step();
    t.harness.step();
    t.harness.get_by_label("Delete main…").click();
    t.harness.step();
    assert!(t.app().confirm.is_none());
}

#[test]
fn create_and_delete_lightweight_and_annotated_tags() {
    let mut t = TestApp::engine();
    let (dir, _repo) = temp_repo();
    t.add_and_open(dir.path());

    // Lightweight from the TAGS `+`.
    t.harness.step();
    branches(&mut t);
    t.harness.get_by_label("Create tag on HEAD…").click();
    t.harness.step();
    let d = t.app().history_ui.create_tag.as_mut().unwrap_or_else(|| panic!("dialog"));
    assert!(d.annotated && d.remote.is_none());
    d.annotated = false;
    d.name = "v1".into();
    t.harness.step();
    button(&mut t, "Create tag");
    t.settle_until("v1", |app| {
        app.store
            .repo
            .as_ref()
            .is_some_and(|r| r.commits.tag("v1").is_some_and(|t| !t.is_annotated()))
    });
    // The create command's completion (which closes its dialog) may still be pending; let it run before the next
    // dialog opens.
    t.settle();

    // Annotated without a signature: nothing is created; the dialog stays open.
    super::open_create_tag(t.app(), None);
    if let Some(d) = t.app().history_ui.create_tag.as_mut() {
        d.name = "v2".into();
        d.message = "Release 2".into();
    }
    t.harness.step();
    t.harness.get_by_label("Annotated tags need your name and email.");
    button(&mut t, "Create tag");
    t.settle();
    // Integrated flow (WP15): the signature dialog opens on top; nothing is created yet.
    assert!(t.app().history_ui.create_tag.is_some());
    assert!(t.app().settings_ui.signature_dialog_open());
    assert!(t.app().store.repo.as_ref().is_some_and(|r| r.commits.tag("v2").is_none()));

    // Saving the signature retries the tag.
    // The tag dialog behind also has a "Name" field: the signature dialog's is drawn last (top modal).
    for (label, text) in [("Name", "Test"), ("Email", "test@example.com")] {
        top_node(&t, label).focus();
        t.harness.step();
        top_node(&t, label).type_text(text);
        t.harness.step();
    }
    t.harness.get_by_label("Save").click();
    t.settle_until("v2", |app| {
        app.history_ui.create_tag.is_none()
            && app
                .store
                .repo
                .as_ref()
                .is_some_and(|r| r.commits.tag("v2").is_some_and(|t| t.message().contains("Release 2")))
    });

    // Delete from the sidebar menu (confirmed).
    t.harness.step();
    context_click(&mut t, "v1", "Delete local tag…");
    button(&mut t, "Delete tag");
    t.settle_until("v1 deleted", |app| {
        app.store
            .repo
            .as_ref()
            .is_some_and(|r| r.commits.tag("v1").is_none() && r.commits.tag("v2").is_some())
    });
}

#[test]
fn tags_push_and_delete_on_a_local_bare_remote() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    let remote_dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let bare = git2::Repository::init_bare(remote_dir.path()).unwrap_or_else(|e| panic!("{e}"));
    repo.remote("origin", &remote_dir.path().to_string_lossy())
        .unwrap_or_else(|e| panic!("{e}"));
    let head = repo.head().and_then(|h| h.peel_to_commit()).unwrap_or_else(|e| panic!("{e}"));
    repo.tag_lightweight("v1", head.as_object(), false)
        .unwrap_or_else(|e| panic!("{e}"));
    t.add_and_open(dir.path());
    t.settle_until("remotes", |app| app.store.repo.as_ref().is_some_and(|r| r.remotes.len() == 1));

    // Opening a tag menu loads the remote tag lists (once).
    branches(&mut t);
    t.harness.get_by_label("v1").click_secondary();
    t.harness.step();
    t.settle_until("remote tags loaded", |app| {
        app.tab()
            .is_some_and(|tab| matches!(&tab.history.remote_tags, RemoteTags::Loaded(m) if m.get("origin").is_some_and(Vec::is_empty)))
    });
    t.harness.key_press(egui::Key::Escape);
    t.harness.step();

    run(
        t.app(),
        vec![Intent::PushTag {
            remote: "origin".into(),
            name: "v1".into(),
        }],
    );
    t.settle_until("pushed and reloaded", |app| {
        app.tab()
            .is_some_and(|tab| matches!(&tab.history.remote_tags, RemoteTags::Loaded(m) if m.get("origin").is_some_and(|v| v == &["v1"])))
    });
    assert!(bare.find_reference("refs/tags/v1").is_ok());

    t.harness.step();
    context_click(&mut t, "v1", "Delete tag from origin…");
    button(&mut t, "Delete from origin");
    t.settle_until("deleted on the remote", |app| {
        app.tab()
            .is_some_and(|tab| matches!(&tab.history.remote_tags, RemoteTags::Loaded(m) if m.get("origin").is_some_and(Vec::is_empty)))
    });
    assert!(bare.find_reference("refs/tags/v1").is_err());
    // The local tag is kept.
    assert!(t.app().store.repo.as_ref().is_some_and(|r| r.commits.tag("v1").is_some()));

    // The create-tag dialog can push the new tag to origin.
    super::open_create_tag(t.app(), None);
    if let Some(d) = t.app().history_ui.create_tag.as_mut() {
        assert_eq!(d.remote.as_deref(), Some("origin"));
        d.annotated = false;
        d.name = "v9".into();
        d.push = true;
    }
    t.harness.step();
    t.harness.get_by_label("Push to origin");
    button(&mut t, "Create tag");
    t.settle_until("v9 pushed", |_| bare.find_reference("refs/tags/v9").is_ok());
}

fn stash(repo: &mut git2::Repository, file: &str, content: &str, message: &str) {
    write_file(repo, file, content);
    repo.stash_save(&sig(), message, None).unwrap_or_else(|e| panic!("{e}"));
}

fn stash_count(t: &mut TestApp) -> usize {
    t.app().store.repo.as_ref().map_or(0, |r| r.stashes.len())
}

#[test]
fn stash_apply_pop_and_delete_from_the_sidebar() {
    let mut t = TestApp::engine();
    let (dir, mut repo) = temp_repo();
    stash(&mut repo, "README.md", "first stash\n", "first");
    stash(&mut repo, "README.md", "second stash\n", "second");
    t.add_and_open(dir.path());
    t.settle_until("stashes", |app| app.store.repo.as_ref().is_some_and(|r| r.stashes.len() == 2));
    let readme = dir.path().join("README.md");
    let labels: Vec<String> = t
        .app()
        .store
        .repo
        .as_ref()
        .map(|r| {
            r.stashes
                .iter()
                .map(|s| format!("{} {}", s.stash.revspec(), s.stash.message))
                .collect()
        })
        .unwrap_or_default();

    // Apply stash@{0} (kept).
    t.harness.step();
    context_click(&mut t, &labels[0], "Apply");
    t.settle_until("applied", |app| app.store.repo.as_ref().is_some_and(|r| r.has_wip()));
    assert_eq!(std::fs::read_to_string(&readme).ok().as_deref(), Some("second stash\n"));
    assert_eq!(stash_count(&mut t), 2);

    // Clean up, then pop stash@{1} (the older one): applied and dropped.
    std::fs::write(&readme, "hello\n").unwrap_or_else(|e| panic!("{e}"));
    refresh(&mut t, Refresh::FILES);
    t.harness.step();
    context_click(&mut t, &labels[1], "Pop");
    t.settle_until("popped", |app| app.store.repo.as_ref().is_some_and(|r| r.stashes.len() == 1));
    assert_eq!(std::fs::read_to_string(&readme).ok().as_deref(), Some("first stash\n"));

    // Delete the remaining one (confirmed).
    t.harness.step();
    let remaining = t
        .app()
        .store
        .repo
        .as_ref()
        .and_then(|r| r.stashes.first().map(|s| format!("{} {}", s.stash.revspec(), s.stash.message)))
        .unwrap_or_default();
    context_click(&mut t, &remaining, "Delete…");
    assert!(t.app().confirm.as_ref().is_some_and(|c| c.body.starts_with("Delete stash@{0} '")));
    button(&mut t, "Delete stash");
    t.settle_until("deleted", |app| app.store.repo.as_ref().is_some_and(|r| r.stashes.is_empty()));
}

#[test]
fn stash_details_open_stash_diffs_and_popping_falls_back_to_wip() {
    let mut t = TestApp::engine();
    let (dir, mut repo) = temp_repo();
    stash(&mut repo, "README.md", "stashed\n", "my stash");
    t.add_and_open(dir.path());
    t.settle_until("stash", |app| app.store.repo.as_ref().is_some_and(|r| r.stashes.len() == 1));
    let (id, label) = t
        .app()
        .store
        .repo
        .as_ref()
        .and_then(|r| {
            r.stashes
                .first()
                .map(|s| (s.stash.id.clone(), format!("{} {}", s.stash.revspec(), s.stash.message)))
        })
        .unwrap_or_else(|| panic!("stash"));
    branches(&mut t);
    t.harness.get_by_label(&label).click();
    t.harness.step();
    assert_eq!(t.app().tab().map(|tab| tab.selection.clone()), Some(Selection::Stash(id.clone())));
    t.harness.step();
    t.harness.get_by_label("README.md").click();
    t.harness.step();
    match t.app().tab().map(|tab| tab.center.clone()) {
        Some(Center::Diff(request)) => {
            assert_eq!(request.target, DiffTarget::Stash(id));
            assert_eq!(request.file.path, std::path::Path::new("README.md"));
        }
        other => panic!("expected a stash diff, got {other:?}"),
    }
    crate::ui::diff::close(t.app());
    t.harness.step();
    t.harness.get_by_label("Pop").click();
    t.settle_until("popped and back to WIP", |app| {
        app.store.repo.as_ref().is_some_and(|r| r.stashes.is_empty()) && app.tab().is_some_and(|tab| tab.selection == Selection::Wip)
    });
}

#[test]
fn commit_details_load_body_and_files_and_open_a_commit_diff() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    write_file(&repo, "src/lib.rs", "pub fn x() {}\n");
    let second = commit_all(&repo, "Add the library\n\nLonger explanation of the change.\n");
    t.add_and_open(dir.path());
    t.harness.step();
    t.harness.get_by_label("Add the library").click();
    t.harness.step();
    let oid = gityas_core::Oid::from_hex(&second.to_string()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(t.app().tab().map(|tab| tab.selection.clone()), Some(Selection::Commit(oid.clone())));
    t.settle_until("body and files", |app| {
        matches!(app.history_ui.bodies.get(&oid), Some(super::Loadable::Ready(_)))
            && matches!(app.history_ui.files.get(&oid), Some(super::Loadable::Ready(_)))
    });
    t.harness.step();
    t.harness.get_by_label("Longer explanation of the change.");
    t.harness.get_by_label("1 file changed");
    t.harness.get_by_label("src/lib.rs").click();
    t.harness.step();
    match t.app().tab().map(|tab| tab.center.clone()) {
        Some(Center::Diff(request)) => {
            assert_eq!(request.target, DiffTarget::Commit(oid.clone()));
            assert_eq!(request.file.path, std::path::Path::new("src/lib.rs"));
        }
        other => panic!("expected a commit diff, got {other:?}"),
    }
    crate::ui::diff::close(t.app());
    t.harness.step();

    // The parent link selects the parent; ✕ goes back to the working tree.
    let parent = t
        .app()
        .store
        .repo
        .as_ref()
        .and_then(|r| r.commits.commits.get(1).map(|c| c.id.clone()));
    let parent = parent.unwrap_or_else(|| panic!("parent"));
    t.harness.get_by_label(&parent.short(7)).click();
    t.harness.step();
    assert_eq!(t.app().tab().map(|tab| tab.selection.clone()), Some(Selection::Commit(parent)));
    t.harness.get_by_label("Back to the working tree").click();
    t.harness.step();
    assert_eq!(t.app().tab().map(|tab| tab.selection.clone()), Some(Selection::Wip));
}

#[test]
fn checkout_this_commit_detaches_head_and_shows_a_head_pill() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    write_file(&repo, "b.txt", "b\n");
    commit_all(&repo, "second commit");
    t.add_and_open(dir.path());
    t.harness.step();
    context_click(&mut t, "initial commit", "Checkout this commit");
    t.settle_until("detached", |app| {
        app.store.repo.as_ref().and_then(|r| r.head.as_ref()).is_some_and(Head::is_detached)
    });
    t.harness.step();
    t.harness.get_by_label("HEAD pill");
}

#[test]
fn keyboard_and_search_move_the_selection() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    write_file(&repo, "b.txt", "b\n");
    commit_all(&repo, "second commit");
    write_file(&repo, "c.txt", "c\n");
    commit_all(&repo, "third commit");
    t.add_and_open(dir.path());
    t.harness.step();
    let ids: Vec<_> = t
        .app()
        .store
        .repo
        .as_ref()
        .map(|r| r.commits.commits.iter().map(|c| c.id.clone()).collect())
        .unwrap_or_default();
    // Clean tree: no WIP row, ↓ selects the first commit, then the next.
    t.harness.key_press(egui::Key::ArrowDown);
    t.harness.step();
    assert_eq!(
        t.app().tab().map(|tab| tab.selection.clone()),
        Some(Selection::Commit(ids[0].clone()))
    );
    t.harness.key_press(egui::Key::ArrowDown);
    t.harness.step();
    assert_eq!(
        t.app().tab().map(|tab| tab.selection.clone()),
        Some(Selection::Commit(ids[1].clone()))
    );

    // Cmd+F opens the search; Enter jumps to the next match.
    t.harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::F);
    t.harness.step();
    t.harness.step();
    assert!(t.app().tab().is_some_and(|tab| tab.history.search_open));
    if let Some(tab) = t.app().tab_mut() {
        tab.history.search = "initial".into();
    }
    t.harness.step();
    t.harness.key_press(egui::Key::Enter);
    t.harness.step();
    t.harness.step();
    assert_eq!(
        t.app().tab().map(|tab| tab.selection.clone()),
        Some(Selection::Commit(ids[2].clone()))
    );
}

#[test]
fn wip_row_shows_the_draft_summary_and_counts() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    t.add_and_open(dir.path());
    write_file(&repo, "new.txt", "x\n");
    refresh(&mut t, Refresh::FILES);
    t.settle_until("wip", |app| app.store.repo.as_ref().is_some_and(|r| r.has_wip()));
    t.harness.step();
    t.harness.get_by_label("// WIP Work in progress");
    if let Some(tab) = t.app().tab_mut() {
        tab.worktree.summary = "my draft".into();
    }
    t.harness.step();
    t.harness.get_by_label("// WIP my draft").click();
    t.harness.step();
    assert_eq!(t.app().tab().map(|tab| tab.selection.clone()), Some(Selection::Wip));
}

#[test]
fn empty_repository_says_no_commits_yet() {
    let mut t = TestApp::engine();
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    git2::Repository::init(dir.path()).unwrap_or_else(|e| panic!("{e}"));
    let engine = t.app().bridge.engine().clone();
    let registered = t
        .block_on(engine.add_repository(dir.path().to_path_buf(), None))
        .unwrap_or_else(|e| panic!("{e}"));
    t.settle_until("registered", |app| app.store.repository(&registered.id).is_some());
    crate::ui::repos::switch_to(t.app(), registered.id.clone());
    t.settle_until("opened", |app| app.store.repo.as_ref().is_some_and(|r| r.is_ready()));
    t.harness.step();
    assert!(t.app().store.repo.as_ref().is_some_and(|r| r.commits.is_empty()));
    // The TAGS `+` is disabled on an unborn HEAD.
    branches(&mut t);
    t.harness.get_by_label("Create tag on HEAD…").click();
    t.harness.step();
    assert!(t.app().history_ui.create_tag.is_none());
}

#[test]
fn a_selected_commit_that_disappears_falls_back_to_wip() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    write_file(&repo, "b.txt", "b\n");
    let second = commit_all(&repo, "second commit");
    t.add_and_open(dir.path());
    let oid = gityas_core::Oid::from_hex(&second.to_string()).unwrap_or_else(|e| panic!("{e}"));
    if let Some(tab) = t.app().tab_mut() {
        tab.selection = Selection::Commit(oid);
    }
    // Rewrite history: main back to the first commit.
    let first = repo.find_commit(second).and_then(|c| c.parent(0)).unwrap_or_else(|e| panic!("{e}"));
    repo.reset(first.as_object(), git2::ResetType::Hard, None)
        .unwrap_or_else(|e| panic!("{e}"));
    refresh(&mut t, Refresh::ALL);
    t.settle_until("selection reset", |app| {
        app.store.repo.as_ref().is_some_and(|r| r.commits.len() == 1) && app.tab().is_some_and(|tab| tab.selection == Selection::Wip)
    });
}

#[test]
fn mock_mode_smoke() {
    let mut t = TestApp::mock();
    branches(&mut t);
    for label in ["LOCAL", "REMOTE", "TAGS", "STASHES", "// WIP Work in progress", "feature/"] {
        t.harness.get_by_label(label);
    }
    t.harness.get_by_label("Merge branch 'feature/egui-frontend'").click();
    t.harness.step();
    t.harness.step();
    t.harness.step();
    assert!(matches!(t.app().tab().map(|tab| &tab.selection), Some(Selection::Commit(_))));
    assert!(t.app().layout.showing(SidePage::Changes), "selecting a commit shows its details");
    t.harness.get_by_label("commit:");
    // Collapsing a section hides its rows.
    branches(&mut t);
    t.harness.get_by_label("LOCAL").click();
    t.harness.step();
    assert!(t.harness.query_by_label("feature/").is_none());
    t.harness.get_by_label("LOCAL").click();
    t.harness.step();
    // Stash details.
    t.harness
        .get_by_label("stash@{0} WIP on main: experiment with libgit2 blame")
        .click();
    t.harness.step();
    t.harness.step();
    t.harness.step();
    t.harness.get_by_label("stash:");
}

#[test]
#[ignore = "needs a GPU"]
fn snapshot_history_screens() {
    let mut t = TestApp::mock_gpu();
    t.harness.step();
    t.harness.get_by_label("Merge branch 'feature/egui-frontend'").click();
    t.harness.step();
    t.harness.step();
    t.snapshot("history_commit_selected");
}

/// The last node with `label`: when two modals are stacked, the top one is drawn (and listed) last.
fn top_node<'a>(t: &'a TestApp, label: &'a str) -> egui_kittest::Node<'a> {
    t.harness
        .query_all_by_label(label)
        .last()
        .unwrap_or_else(|| panic!("no {label:?} field"))
}

/// A long message pushes the file list below the panel: the whole details body scrolls to reach it.
#[test]
fn commit_details_scroll_to_files_below_a_long_message() {
    let mut t = TestApp::engine();
    let (dir, repo) = temp_repo();
    for i in 0..40 {
        write_file(&repo, &format!("f{i:02}.txt"), "x\n");
    }
    let body: String = (0..80).map(|i| format!("Line {i} of a long explanation.\n")).collect();
    let oid = commit_all(&repo, &format!("Many files\n\n{body}"));
    t.add_and_open(dir.path());
    t.harness.step();
    t.harness.get_by_label("Many files").click();
    t.harness.step();
    let oid = gityas_core::Oid::from_hex(&oid.to_string()).unwrap_or_else(|e| panic!("{e}"));
    t.settle_until("body and files", |app| {
        matches!(app.history_ui.bodies.get(&oid), Some(super::Loadable::Ready(_)))
            && matches!(app.history_ui.files.get(&oid), Some(super::Loadable::Ready(_)))
    });
    t.harness.step();
    // Off-screen rows are not painted (no accessibility node either).
    assert!(t.harness.query_by_label("f39.txt").is_none());
    let close = t.harness.get_by_label("Back to the working tree").rect();
    let panel = egui::pos2(close.left() - 100.0, 400.0);
    t.harness.hover_at(panel);
    for _ in 0..20 {
        t.harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -400.0),
            modifiers: egui::Modifiers::NONE,
            phase: egui::TouchPhase::Move,
        });
        t.harness.step();
    }
    t.harness.run_steps(30);
    t.harness.get_by_label("f39.txt").click();
    t.harness.step();
    assert!(matches!(t.app().tab().map(|tab| tab.center.clone()), Some(Center::Diff(_))));
}

#[test]
fn hovering_a_commit_shows_quick_actions() {
    let mut t = TestApp::mock();
    t.harness.step();
    t.harness.get_by_label("feat: commit graph").hover();
    t.harness.step();
    t.harness.step();
    t.harness.get_by_label("New branch from this commit").click();
    t.harness.step();
    t.harness.step();
    assert!(t.app().history_ui.create_branch_open());
    t.harness.key_press(egui::Key::Escape);
    t.harness.step();
    t.harness.get_by_label("feat: commit graph").hover();
    t.harness.step();
    t.harness.get_by_label("Copy SHA").click();
    t.harness.step();
    let id = t
        .app()
        .store
        .repo
        .as_ref()
        .and_then(|r| r.commits.commits.iter().find(|c| c.summary == "feat: commit graph"))
        .map(|c| c.id.clone())
        .unwrap_or_else(|| panic!("no commit"));
    let copied = t.harness.ctx.data(|d| d.get_temp::<f64>(egui::Id::new(("graph_copied", &id))));
    assert!(copied.is_some(), "the row shows Copied");
}

#[test]
fn a_pill_jumps_back_to_head_once_it_is_scrolled_out_of_view() {
    const JUMP: &str = "Scroll to HEAD";
    let mut t = TestApp::mock();
    t.harness.step();
    assert!(t.harness.query_by_label(JUMP).is_none(), "HEAD is in view");
    let oldest = t
        .app()
        .store
        .repo
        .as_ref()
        .and_then(|r| r.commits.commits.last())
        .map(|c| c.id.clone());
    if let Some(tab) = t.app().tab_mut() {
        tab.history.scroll_to = oldest;
    }
    t.harness.step();
    t.harness.step();
    t.harness.get_by_label(JUMP).click();
    t.harness.step();
    t.harness.step();
    t.harness.step();
    assert!(t.harness.query_by_label(JUMP).is_none(), "HEAD is back in view");
}
