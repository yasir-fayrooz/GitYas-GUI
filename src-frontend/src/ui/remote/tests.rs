//! Headless UI tests: the real app on an in-memory engine against temp repositories, with a local bare repository
//! as `origin` and a second clone of it playing "another user".

use std::path::Path;

use egui_kittest::kittest::Queryable;
use gityas_core::models::{CommitSignature, RemoteOp, RemoteProgress};
use gityas_engine::Refresh;
use tempfile::TempDir;

use super::*;
use crate::testutil::{TestApp, commit_all, temp_repo, write_file};

/// Work repository (`main`, one commit) with a bare `origin` that has `main` and the upstream set.
struct Fixture {
    work_dir: TempDir,
    work: git2::Repository,
    bare_dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let (work_dir, work) = temp_repo();
        let bare_dir = new_bare();
        work.remote("origin", path_str(bare_dir.path())).unwrap_or_else(|e| panic!("{e}"));
        git2_push(&work, "origin", "refs/heads/main:refs/heads/main");
        work.find_branch("main", git2::BranchType::Local)
            .and_then(|mut b| b.set_upstream(Some("origin/main")))
            .unwrap_or_else(|e| panic!("upstream: {e}"));
        Self { work_dir, work, bare_dir }
    }

    /// A second clone of `origin` ("another user").
    fn other(&self) -> (TempDir, git2::Repository) {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let repo = git2::Repository::clone(path_str(self.bare_dir.path()), dir.path()).unwrap_or_else(|e| panic!("clone: {e}"));
        (dir, repo)
    }

    /// Another user commits `rel = content` on main and pushes it.
    fn remote_commit(&self, rel: &str, content: &str) -> git2::Oid {
        let (_dir, other) = self.other();
        write_file(&other, rel, content);
        let oid = commit_all(&other, &format!("theirs: {rel}"));
        git2_push(&other, "origin", "refs/heads/main:refs/heads/main");
        oid
    }

    fn bare(&self) -> git2::Repository {
        git2::Repository::open_bare(self.bare_dir.path()).unwrap_or_else(|e| panic!("{e}"))
    }

    fn open(&self) -> TestApp {
        let mut t = TestApp::engine();
        t.add_and_open(self.work_dir.path());
        t.settle_until("remotes and branches loaded", |app| {
            app.store
                .repo
                .as_ref()
                .is_some_and(|r| r.loaded.remotes && r.loaded.local_branches && r.loaded.status && !r.remotes.is_empty())
        });
        t
    }
}

fn new_bare() -> TempDir {
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let bare = git2::Repository::init_bare(dir.path()).unwrap_or_else(|e| panic!("{e}"));
    bare.set_head("refs/heads/main").unwrap_or_else(|e| panic!("{e}"));
    dir
}

fn path_str(path: &Path) -> &str {
    path.to_str().unwrap_or_else(|| panic!("non-UTF-8 temp path"))
}

fn git2_push(repo: &git2::Repository, remote: &str, refspec: &str) {
    repo.find_remote(remote)
        .and_then(|mut r| r.push(&[refspec], None))
        .unwrap_or_else(|e| panic!("push: {e}"));
}

fn titles(t: &mut TestApp) -> Vec<String> {
    t.app().notify.toasts().map(|t| t.title.clone()).collect()
}

fn set_signature(t: &mut TestApp) {
    let engine = t.app().bridge.engine().clone();
    t.block_on(engine.update_commit_signature(CommitSignature::new("Test", "test@example.com")))
        .unwrap_or_else(|e| panic!("{e}"));
    t.settle();
}

fn refresh(t: &mut TestApp) {
    let engine = t.app().bridge.engine().clone();
    t.block_on(engine.refresh(Refresh::ALL)).unwrap_or_else(|e| panic!("{e}"));
    t.settle();
}

fn head_oid(repo: &git2::Repository) -> Option<git2::Oid> {
    repo.head().ok().and_then(|h| h.target())
}

/// Commit on `main` in the work repo touching README.md (conflicts with a remote change to the same line).
fn local_readme_commit(f: &Fixture, content: &str) -> git2::Oid {
    write_file(&f.work, "README.md", content);
    commit_all(&f.work, "ours: README")
}

#[test]
fn fetch_nothing_new_flashes_then_updates_toast() {
    let f = Fixture::new();
    let mut t = f.open();
    fetch_all(t.app());
    assert!(t.app().bridge.is_running(TASK_FETCH));
    t.settle();
    assert!(titles(&mut t).is_empty(), "{:?}", titles(&mut t));
    assert_eq!(t.app().remote_ui.flash.as_ref().map(|(m, _)| m.as_str()), Some("Up to date"));
    t.harness.get_by_label_contains("Up to date");

    f.remote_commit("theirs.txt", "x\n");
    fetch_all(t.app());
    t.settle();
    assert_eq!(titles(&mut t), ["Fetched origin: 1 branch updated"]);
    assert!(t.app().store.progress.is_none(), "completion clears the progress");
    t.settle_until("behind by one", |app| {
        app.store.repo.as_ref().and_then(|r| r.head_branch()).is_some_and(|b| b.behind == 1)
    });
}

#[test]
fn pull_fast_forward() {
    let f = Fixture::new();
    let theirs = f.remote_commit("theirs.txt", "x\n");
    let mut t = f.open();
    pull_current(t.app());
    t.settle();
    let short = theirs.to_string()[..7].to_owned();
    assert_eq!(titles(&mut t), [format!("Fast-forwarded main to {short}")]);
    assert_eq!(head_oid(&f.work), Some(theirs));

    pull_current(t.app());
    t.settle();
    assert_eq!(titles(&mut t).last().map(String::as_str), Some("Already up to date"));
}

#[test]
fn pull_with_conflicts_shows_the_banner_and_abort_is_confirmed() {
    let f = Fixture::new();
    f.remote_commit("README.md", "theirs\n");
    local_readme_commit(&f, "ours\n");
    let mut t = f.open();
    set_signature(&mut t);
    pull_current(t.app());
    t.settle();
    assert_eq!(titles(&mut t), ["Merge has conflicts in 1 file"]);
    t.settle_until("merge state and conflicted file", |app| {
        app.store
            .repo
            .as_ref()
            .is_some_and(|r| r.status.is_merging() && text::conflicted_count(&r.files) == 1)
    });
    t.harness.step();
    t.harness.get_by_label("Merging origin/main into main — 1 conflicted file");

    // The banner button opens the confirmation; nothing happens until it is confirmed.
    t.harness.get_by_label("Abort merge").click();
    t.harness.step();
    t.harness.step();
    assert!(t.app().confirm.is_some());
    t.harness
        .get_by_label("Abort the merge? Files changed by the merge are restored; your other changes are kept.");
    assert!(t.app().store.repo.as_ref().is_some_and(|r| r.status.is_merging()));
    let buttons = t.harness.get_all_by_label("Abort merge").count();
    assert_eq!(buttons, 3, "banner button + confirm title + confirm button");
    t.harness
        .get_all_by_label("Abort merge")
        .last()
        .unwrap_or_else(|| panic!("confirm"))
        .click();
    t.harness.step();
    t.settle();
    t.settle_until("merge aborted", |app| {
        app.store.repo.as_ref().is_some_and(|r| !r.status.is_merging() && !r.has_wip())
    });
    assert_eq!(titles(&mut t).last().map(String::as_str), Some("Merge aborted"));
    t.harness.step();
    assert!(t.harness.query_by_label_contains("Merging origin/main").is_none());
    let readme = std::fs::read_to_string(f.work_dir.path().join("README.md")).unwrap_or_default();
    assert_eq!(readme, "ours\n");
}

#[test]
fn push_with_upstream() {
    let f = Fixture::new();
    let ours = local_readme_commit(&f, "ours\n");
    let mut t = f.open();
    push_current(t.app());
    assert!(t.app().bridge.is_running(TASK_PUSH));
    t.settle();
    assert_eq!(titles(&mut t), ["Pushed main to origin"]);
    assert_eq!(f.bare().find_reference("refs/heads/main").ok().and_then(|r| r.target()), Some(ours));
}

#[test]
fn push_branch_without_upstream_single_remote_sets_upstream() {
    let f = Fixture::new();
    let head = f.work.head().and_then(|h| h.peel_to_commit()).unwrap_or_else(|e| panic!("{e}"));
    f.work.branch("side", &head, false).unwrap_or_else(|e| panic!("{e}"));
    let mut t = f.open();
    push(t.app(), "refs/heads/side".into());
    assert!(t.app().remote_ui.push_dialog.is_none(), "one remote: no dialog");
    t.settle();
    assert_eq!(titles(&mut t), ["Pushed side to origin and set upstream"]);
    assert!(f.bare().find_reference("refs/heads/side").is_ok());
    t.settle_until("side tracks origin/side", |app| {
        app.store.repo.as_ref().is_some_and(|r| {
            r.local_branches
                .iter()
                .any(|b| b.name == "side" && b.upstream.as_deref() == Some("origin/side"))
        })
    });
}

#[test]
fn push_branch_without_upstream_two_remotes_asks_in_a_dialog() {
    let f = Fixture::new();
    let fork = new_bare();
    f.work.remote("fork", path_str(fork.path())).unwrap_or_else(|e| panic!("{e}"));
    let head = f.work.head().and_then(|h| h.peel_to_commit()).unwrap_or_else(|e| panic!("{e}"));
    f.work.branch("side", &head, false).unwrap_or_else(|e| panic!("{e}"));
    let mut t = f.open();
    t.settle_until("two remotes", |app| app.store.repo.as_ref().is_some_and(|r| r.remotes.len() == 2));

    push(t.app(), "refs/heads/side".into());
    t.harness.step();
    t.harness.step();
    {
        let dialog = t.app().remote_ui.push_dialog.as_ref().unwrap_or_else(|| panic!("dialog"));
        assert_eq!((dialog.remote.as_str(), dialog.name.as_str()), ("origin", "side"));
    }
    t.harness.get_by_label("Push side");
    // Pick the other remote and a different name (what the combo box and text field do).
    if let Some(dialog) = t.app().remote_ui.push_dialog.as_mut() {
        dialog.remote = "fork".into();
        dialog.name = "renamed".into();
    }
    t.harness.step();
    t.harness
        .get_all_by_label("Push")
        .last()
        .unwrap_or_else(|| panic!("Push button"))
        .click();
    t.harness.step();
    t.settle();
    assert_eq!(titles(&mut t), ["Pushed side to fork as renamed and set upstream"]);
    assert!(t.app().remote_ui.push_dialog.is_none());
    let fork_repo = git2::Repository::open_bare(fork.path()).unwrap_or_else(|e| panic!("{e}"));
    assert!(fork_repo.find_reference("refs/heads/renamed").is_ok());
    assert!(f.bare().find_reference("refs/heads/side").is_err());

    // Publishing again under an existing name: the engine refuses, the error stays inline in the dialog.
    f.work.branch("other", &head, false).unwrap_or_else(|e| panic!("{e}"));
    refresh(&mut t);
    push(t.app(), "refs/heads/other".into());
    if let Some(dialog) = t.app().remote_ui.push_dialog.as_mut() {
        dialog.remote = "fork".into();
        dialog.name = "renamed".into();
    }
    t.harness.step();
    t.harness
        .get_all_by_label("Push")
        .last()
        .unwrap_or_else(|| panic!("Push button"))
        .click();
    t.harness.step();
    t.settle();
    let error = t.app().remote_ui.push_dialog.as_ref().and_then(|d| d.error.clone());
    assert!(error.as_deref().is_some_and(|e| e.contains("already exists")), "{error:?}");
    assert!(titles(&mut t).len() == 1, "no new toast: {:?}", titles(&mut t));
}

#[test]
fn rejected_push_offers_pull() {
    let f = Fixture::new();
    f.remote_commit("theirs.txt", "x\n");
    write_file(&f.work, "ours.txt", "y\n");
    commit_all(&f.work, "ours");
    let mut t = f.open();
    set_signature(&mut t);
    push_current(t.app());
    t.settle();
    assert_eq!(titles(&mut t), ["Push rejected: origin/main has new commits. Pull first."]);
    let action = t
        .app()
        .notify
        .toasts()
        .next()
        .and_then(|t| t.action.as_ref())
        .map(|a| a.label.clone());
    assert_eq!(action.as_deref(), Some("Pull"));
    t.harness.get_by_label("Pull").click();
    t.harness.step();
    t.settle();
    assert_eq!(titles(&mut t), ["Merged origin/main into main"]);
    // Now the push goes through.
    push_current(t.app());
    t.settle();
    assert_eq!(titles(&mut t).last().map(String::as_str), Some("Pushed main to origin"));
}

#[test]
fn merge_into_current_fast_forward_signature_and_conflicts() {
    let (dir, repo) = temp_repo();
    let base = repo.head().and_then(|h| h.peel_to_commit()).unwrap_or_else(|e| panic!("{e}"));
    // feature: one commit ahead of main (fast-forward).
    repo.branch("feature", &base, false).unwrap_or_else(|e| panic!("{e}"));
    repo.set_head("refs/heads/feature").unwrap_or_else(|e| panic!("{e}"));
    write_file(&repo, "feature.txt", "f\n");
    let feature = commit_all(&repo, "feature");
    // clash: changes README differently from what main will have.
    repo.branch("clash", &repo.find_commit(feature).unwrap_or_else(|e| panic!("{e}")), false)
        .unwrap_or_else(|e| panic!("{e}"));
    repo.set_head("refs/heads/clash").unwrap_or_else(|e| panic!("{e}"));
    write_file(&repo, "README.md", "clash\n");
    commit_all(&repo, "clash");
    repo.set_head("refs/heads/main").unwrap_or_else(|e| panic!("{e}"));
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap_or_else(|e| panic!("{e}"));

    let mut t = TestApp::engine();
    t.add_and_open(dir.path());
    merge_into_current(t.app(), "refs/heads/feature".into());
    t.settle();
    assert_eq!(titles(&mut t), [format!("Fast-forwarded main to {}", &feature.to_string()[..7])]);

    write_file(&repo, "README.md", "main\n");
    commit_all(&repo, "main");
    refresh(&mut t);
    // A real merge needs a signature: no error toast, the signature dialog is requested (WP15) instead.
    merge_into_current(t.app(), "refs/heads/clash".into());
    t.settle();
    assert_eq!(titles(&mut t).len(), 1, "{:?}", titles(&mut t));
    assert!(t.app().store.repo.as_ref().is_some_and(|r| !r.status.is_merging()));

    set_signature(&mut t);
    merge_into_current(t.app(), "refs/heads/clash".into());
    t.settle();
    assert_eq!(titles(&mut t).last().map(String::as_str), Some("Merge has conflicts in 1 file"));
    t.settle_until("merge banner", |app| app.store.repo.as_ref().is_some_and(|r| r.status.is_merging()));
    t.harness.step();
    t.harness.get_by_label_contains("Merging clash into main");

    // Merging again while the merge is in progress is refused by the engine and reported.
    merge_into_current(t.app(), "refs/heads/feature".into());
    t.settle();
    assert_eq!(titles(&mut t).last().map(String::as_str), Some("Could not merge feature into main"));
}

#[test]
fn local_changes_blocking_a_merge_offer_stash() {
    let (dir, repo) = temp_repo();
    let base = repo.head().and_then(|h| h.peel_to_commit()).unwrap_or_else(|e| panic!("{e}"));
    repo.branch("feature", &base, false).unwrap_or_else(|e| panic!("{e}"));
    repo.set_head("refs/heads/feature").unwrap_or_else(|e| panic!("{e}"));
    write_file(&repo, "README.md", "feature\n");
    commit_all(&repo, "feature");
    repo.set_head("refs/heads/main").unwrap_or_else(|e| panic!("{e}"));
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap_or_else(|e| panic!("{e}"));
    write_file(&repo, "README.md", "local edit\n");
    let mut t = TestApp::engine();
    t.add_and_open(dir.path());
    merge_into_current(t.app(), "refs/heads/feature".into());
    t.settle();
    let toast = t
        .app()
        .notify
        .toasts()
        .next()
        .map(|t| (t.title.clone(), t.action.as_ref().map(|a| a.label.clone())));
    assert_eq!(
        toast,
        Some(("Could not merge feature into main".into(), Some("Stash changes".into())))
    );
}

#[test]
fn pull_without_upstream_offers_push() {
    let (dir, _repo) = temp_repo();
    let mut t = TestApp::engine();
    t.add_and_open(dir.path());
    t.settle_until("branches", |app| app.store.repo.as_ref().is_some_and(|r| r.loaded.local_branches));
    pull_current(t.app());
    assert!(!t.app().bridge.is_running(TASK_PULL));
    let toast = t
        .app()
        .notify
        .toasts()
        .next()
        .map(|t| (t.title.clone(), t.action.as_ref().map(|a| a.label.clone())));
    assert_eq!(toast, Some(("main has no upstream branch".into(), Some("Push".into()))));
    // And with no remote at all, Push says so instead of failing in the engine.
    push_current(t.app());
    assert_eq!(titles(&mut t).last().map(String::as_str), Some("This repository has no remotes"));
}

#[test]
fn status_bar_shows_progress_and_cancel_while_a_remote_task_runs() {
    let f = Fixture::new();
    let mut t = f.open();
    t.app().remote_ui.set_target(RemoteOp::Fetch, "origin".into());
    // A stand-in for a slow fetch: holds the TASK_FETCH key for a moment.
    t.app().bridge.run(
        TASK_FETCH,
        |_engine| async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            Ok(())
        },
        |_app, _result: gityas_core::Result<()>| {},
    );
    t.app().store.progress = Some((
        None,
        RemoteProgress {
            op: RemoteOp::Fetch,
            percent: Some(45),
            message: "Receiving objects: 45/100".into(),
        },
    ));
    t.harness.step();
    t.harness.get_by_label("Fetching origin… 45 %");
    t.harness.get_by_label("Cancel").click();
    t.harness.step();
    t.settle();
    // The task ended: no progress shown even though the (fake) progress was never cleared.
    assert!(t.harness.query_by_label_contains("Fetching origin").is_none());
}

#[test]
fn warnings_are_dismissible_and_background_auth_offers_credentials() {
    let mut t = TestApp::mock();
    t.app().warnings.push("The user database was recovered".into());
    t.app().warnings.push(
        "Background fetch of remote \"origin\" stopped after 3 failed sign-ins. Check the access token, then reopen the repository.".into(),
    );
    t.harness.step();
    t.harness.get_by_label("The user database was recovered");
    t.harness.get_by_label("Open credentials");
    t.harness
        .get_all_by_label("Dismiss warning")
        .next()
        .unwrap_or_else(|| panic!("no dismiss button"))
        .click();
    t.harness.step();
    t.harness.step();
    assert_eq!(t.app().warnings.len(), 1);
    assert!(t.app().warnings[0].starts_with("Background fetch"));
}

#[test]
fn auth_errors_get_a_provider_title_and_credentials_action() {
    let mut t = TestApp::mock();
    report_remote_error(
        t.app(),
        "Could not fetch",
        &GitYasError::Auth("origin requires authentication".into()),
    );
    report_remote_error(t.app(), "Could not fetch", &GitYasError::Cancelled);
    let toasts: Vec<(String, Option<String>)> = t
        .app()
        .notify
        .toasts()
        .map(|t| (t.title.clone(), t.action.as_ref().map(|a| a.label.clone())))
        .collect();
    // The mock repository is a connected GitHub repository.
    assert_eq!(toasts, [("GitHub rejected the token".into(), Some("Open credentials".into()))]);
    assert_eq!(t.app().remote_ui.flash.as_ref().map(|(m, _)| m.as_str()), Some("Cancelled"));
}

#[test]
#[ignore = "needs a GPU"]
fn snapshot_banner_progress_and_push_dialog() {
    let mut t = TestApp::mock_gpu();
    if let Some(repo) = t.app().store.repo.as_mut() {
        repo.status.operation = gityas_core::models::RepoOperation::Merge;
        repo.status.prepared_message = Some("Merge branch 'feature/x'".into());
    }
    t.app().warnings.push("The user database was recovered from a backup".into());
    // Let the new panel settle (layout passes) before the image is taken.
    for _ in 0..3 {
        t.harness.step();
    }
    t.snapshot("remote_banner");
    t.app().remote_ui.push_dialog = Some(PushDialog {
        refname: "refs/heads/feature/x".into(),
        local: "feature/x".into(),
        remotes: vec!["origin".into(), "fork".into()],
        remote: "origin".into(),
        name: "feature/x".into(),
        error: None,
        autofocus: true,
    });
    t.harness.step();
    t.harness.step();
    t.snapshot("remote_push_dialog");
}
