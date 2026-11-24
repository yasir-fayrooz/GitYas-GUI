//! Linked worktrees: list / add / remove.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test crate: panics are the assertions

mod repo_support;

use gityas_core::GitYasError;
use gityas_core::models::WorktreeBranch;
use gityas_git::worktrees;
use repo_support::*;
use tempfile::TempDir;

fn repo() -> (TempDir, git2::Repository) {
    let tmp = TempDir::new().unwrap();
    let main = tmp.path().join("main");
    std::fs::create_dir(&main).unwrap();
    let repo = init(&main);
    commit_file(&repo, "a.txt", "one\n", "c1");
    (tmp, repo)
}

#[test]
fn a_plain_repository_lists_only_its_main_worktree() {
    let (_tmp, repo) = repo();
    let list = worktrees::list_worktrees(&repo).unwrap();
    assert_eq!(list.len(), 1);
    let main = &list[0];
    assert!(main.is_main() && main.is_current && !main.prunable && !main.locked);
    assert_eq!(main.head, Some(oid(head_oid(&repo))));
    assert!(main.branch.is_some());
}

#[test]
fn add_with_a_new_branch_then_list_from_both_sides() {
    let (tmp, repo) = repo();
    let path = tmp.path().join("main-feature");
    let added = worktrees::add_worktree(
        &repo,
        &path,
        &WorktreeBranch::New {
            name: "feature/x".into(),
            from: None,
        },
    )
    .unwrap();
    assert_eq!(added.name.as_deref(), Some("main-feature"));
    assert_eq!(added.branch.as_deref(), Some("feature/x"));
    assert!(!added.is_current);
    assert!(path.join("a.txt").is_file());

    // From the linked worktree: same list, the linked one is current.
    let linked = git2::Repository::open(&path).unwrap();
    let list = worktrees::list_worktrees(&linked).unwrap();
    assert_eq!(list.len(), 2);
    assert!(list[0].is_main() && !list[0].is_current);
    assert!(list[1].is_current);
}

#[test]
fn add_an_existing_branch_and_refuse_one_checked_out_elsewhere() {
    let (tmp, repo) = repo();
    gityas_git::branches::create_branch(&repo, "topic", None, false).unwrap();
    let topic = WorktreeBranch::Existing {
        refname: "refs/heads/topic".into(),
    };
    worktrees::add_worktree(&repo, &tmp.path().join("t1"), &topic).unwrap();
    let again = worktrees::add_worktree(&repo, &tmp.path().join("t2"), &topic);
    assert!(matches!(again, Err(GitYasError::InvalidInput(_))), "{again:?}");
    assert!(!tmp.path().join("t2").exists());
}

#[test]
fn a_failed_add_deletes_the_branch_it_created() {
    let (tmp, repo) = repo();
    let busy = tmp.path().join("busy");
    std::fs::create_dir(&busy).unwrap();
    std::fs::write(busy.join("x"), "x").unwrap();
    let result = worktrees::add_worktree(
        &repo,
        &busy,
        &WorktreeBranch::New {
            name: "new".into(),
            from: None,
        },
    );
    assert!(matches!(result, Err(GitYasError::AlreadyExists(_))), "{result:?}");
    assert!(repo.find_branch("new", git2::BranchType::Local).is_err());
}

#[test]
fn names_are_unique() {
    let (tmp, repo) = repo();
    let new = |n: &str| WorktreeBranch::New {
        name: n.into(),
        from: None,
    };
    let a = worktrees::add_worktree(&repo, &tmp.path().join("x").join("wt"), &new("a")).unwrap();
    let b = worktrees::add_worktree(&repo, &tmp.path().join("y").join("wt"), &new("b")).unwrap();
    assert_eq!(a.name.as_deref(), Some("wt"));
    assert_eq!(b.name.as_deref(), Some("wt-2"));
}

#[test]
fn remove_refuses_changes_without_force_and_keeps_the_branch() {
    let (tmp, repo) = repo();
    let path = tmp.path().join("wt");
    let wt = worktrees::add_worktree(
        &repo,
        &path,
        &WorktreeBranch::New {
            name: "b".into(),
            from: None,
        },
    )
    .unwrap();
    let name = wt.name.unwrap();
    std::fs::write(path.join("untracked.txt"), "u").unwrap();
    let refused = worktrees::remove_worktree(&repo, &name, false);
    assert!(
        matches!(refused, Err(GitYasError::WorktreeDirty(ref f)) if f == "wt"),
        "{refused:?}"
    );
    assert!(path.exists());

    worktrees::remove_worktree(&repo, &name, true).unwrap();
    assert!(!path.exists());
    assert_eq!(worktrees::list_worktrees(&repo).unwrap().len(), 1);
    assert!(repo.find_branch("b", git2::BranchType::Local).is_ok());
}

#[test]
fn a_deleted_folder_is_prunable_and_removing_it_prunes() {
    let (tmp, repo) = repo();
    let path = tmp.path().join("gone");
    let wt = worktrees::add_worktree(
        &repo,
        &path,
        &WorktreeBranch::New {
            name: "g".into(),
            from: None,
        },
    )
    .unwrap();
    std::fs::remove_dir_all(&path).unwrap();
    let list = worktrees::list_worktrees(&repo).unwrap();
    assert!(list[1].prunable);
    worktrees::remove_worktree(&repo, wt.name.as_deref().unwrap(), false).unwrap();
    assert_eq!(worktrees::list_worktrees(&repo).unwrap().len(), 1);
}

#[test]
fn locked_worktrees_need_force_and_the_open_one_cant_be_removed() {
    let (tmp, repo) = repo();
    let path = tmp.path().join("locked");
    let wt = worktrees::add_worktree(
        &repo,
        &path,
        &WorktreeBranch::New {
            name: "l".into(),
            from: None,
        },
    )
    .unwrap();
    let name = wt.name.unwrap();
    repo.find_worktree(&name).unwrap().lock(Some("on a USB drive")).unwrap();
    let listed = worktrees::list_worktrees(&repo).unwrap();
    assert!(listed[1].locked);
    assert_eq!(listed[1].lock_reason.as_deref(), Some("on a USB drive"));
    assert!(matches!(
        worktrees::remove_worktree(&repo, &name, false),
        Err(GitYasError::InvalidInput(_))
    ));

    let linked = git2::Repository::open(&path).unwrap();
    assert!(matches!(
        worktrees::remove_worktree(&linked, &name, true),
        Err(GitYasError::InvalidInput(_))
    ));
    worktrees::remove_worktree(&repo, &name, true).unwrap();
    assert!(!path.exists());
}
