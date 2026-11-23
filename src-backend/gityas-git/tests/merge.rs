//! Merge (up-to-date / fast-forward / merge commit / conflicts) and abort merge.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test crate: panics are the assertions

mod repo_support;

use std::path::PathBuf;

use gityas_core::GitYasError;
use gityas_git::branches;
use gityas_git::remote::{MergeOutcome, abort_merge, merge_branch};
use gityas_git::repository::{self, RepoState};
use repo_support::*;
use tempfile::TempDir;

/// main: a.txt=one. feature (from main): b.txt added. main then: c.txt added (so the merge is non-trivial).
fn diverged() -> (TempDir, git2::Repository) {
    let tmp = TempDir::new().unwrap();
    let repo = init(tmp.path());
    commit_file(&repo, "a.txt", "one\ntwo\nthree\n", "base");
    branches::create_branch(&repo, "feature", None, true).unwrap();
    commit_file(&repo, "b.txt", "feature\n", "feature work");
    branches::checkout_branch(&repo, "refs/heads/main").unwrap();
    commit_file(&repo, "c.txt", "main\n", "main work");
    (tmp, repo)
}

/// Both branches edit the same line of a.txt.
fn conflicting() -> (TempDir, git2::Repository) {
    let tmp = TempDir::new().unwrap();
    let repo = init(tmp.path());
    commit_file(&repo, "a.txt", "one\ntwo\nthree\n", "base");
    branches::create_branch(&repo, "feature", None, true).unwrap();
    commit_file(&repo, "a.txt", "one\nFEATURE\nthree\n", "feature edit");
    commit_file(&repo, "added-by-feature.txt", "x\n", "feature adds file");
    branches::checkout_branch(&repo, "refs/heads/main").unwrap();
    commit_file(&repo, "a.txt", "one\nMAIN\nthree\n", "main edit");
    (tmp, repo)
}

#[test]
fn up_to_date() {
    let (_tmp, repo) = diverged();
    let base = repo.revparse_single("main~1").unwrap().id();
    repo.reference("refs/heads/old", base, false, "t").unwrap();
    assert_eq!(
        merge_branch(&repo, "refs/heads/old", "refs/heads/main", None).unwrap(),
        MergeOutcome::UpToDate
    );
    assert_eq!(
        merge_branch(&repo, "refs/heads/main", "refs/heads/main", None).unwrap(),
        MergeOutcome::UpToDate
    );
}

#[test]
fn fast_forward_checked_out_branch_keeps_unrelated_changes() {
    let fx = Fixture::new();
    let other = fx.other_clone("other");
    let c2 = commit_file(&other, "b", "b\n", "c2");
    raw_push(&other, "refs/heads/main:refs/heads/main");
    fx.work.find_remote("origin").unwrap().fetch::<&str>(&[], None, None).unwrap();
    write(&fx.work, "a.txt", "local edit\n"); // not touched by the merge
    write(&fx.work, "untracked", "u\n");

    let before = head_oid(&fx.work);
    let out = merge_branch(&fx.work, "refs/remotes/origin/main", "refs/heads/main", None).unwrap();
    assert_eq!(
        out,
        MergeOutcome::FastForward {
            from: Some(oid(before)),
            to: oid(c2)
        }
    );
    assert_eq!(head_oid(&fx.work), c2);
    assert_eq!(read(&fx.work, "b"), "b\n");
    assert_eq!(read(&fx.work, "a.txt"), "local edit\n");
    assert_eq!(
        git(fx.work.workdir().unwrap(), &["status", "--porcelain"]),
        " M a.txt\n?? untracked\n"
    );
}

#[test]
fn fast_forward_blocked_by_conflicting_local_change_modifies_nothing() {
    let fx = Fixture::new();
    let other = fx.other_clone("other");
    commit_file(&other, "a.txt", "theirs\n", "edit a");
    raw_push(&other, "refs/heads/main:refs/heads/main");
    fx.work.find_remote("origin").unwrap().fetch::<&str>(&[], None, None).unwrap();
    write(&fx.work, "a.txt", "mine\n");
    let before = head_oid(&fx.work);
    let err = merge_branch(&fx.work, "refs/remotes/origin/main", "refs/heads/main", None).unwrap_err();
    assert!(
        matches!(&err, GitYasError::Conflict(p) if p.iter().any(|p| p == std::path::Path::new("a.txt"))),
        "{err:?}"
    );
    assert_eq!(head_oid(&fx.work), before);
    assert_eq!(read(&fx.work, "a.txt"), "mine\n");
    assert_eq!(repository::state(&fx.work), RepoState::Clean);
}

/// Bug #2: a symbolic source (`origin/HEAD`) is resolved instead of panicking.
#[test]
fn symbolic_source_ref_is_resolved() {
    let fx = Fixture::new();
    let other = fx.other_clone("other");
    let c2 = commit_file(&other, "b", "b\n", "c2");
    raw_push(&other, "refs/heads/main:refs/heads/main");
    fx.work.find_remote("origin").unwrap().fetch::<&str>(&[], None, None).unwrap();
    fx.work
        .reference_symbolic("refs/remotes/origin/HEAD", "refs/remotes/origin/main", true, "t")
        .unwrap();
    let out = merge_branch(&fx.work, "refs/remotes/origin/HEAD", "refs/heads/main", None).unwrap();
    assert!(matches!(out, MergeOutcome::FastForward { .. }));
    assert_eq!(head_oid(&fx.work), c2);
    // Short names are accepted too; a dangling symbolic ref is a clear error.
    assert_eq!(
        merge_branch(&fx.work, "origin/main", "refs/heads/main", None).unwrap(),
        MergeOutcome::UpToDate
    );
    fx.work
        .reference_symbolic("refs/remotes/origin/DANGLING", "refs/remotes/origin/nope", true, "t")
        .unwrap();
    assert!(matches!(
        merge_branch(&fx.work, "refs/remotes/origin/DANGLING", "refs/heads/main", None),
        Err(GitYasError::NotFound(_))
    ));
    assert!(matches!(
        merge_branch(&fx.work, "refs/heads/nope", "refs/heads/main", None),
        Err(GitYasError::NotFound(_))
    ));
}

#[test]
fn fast_forward_of_branch_not_checked_out_moves_ref_only() {
    let (_tmp, repo) = diverged();
    let feature_tip = repo.refname_to_id("refs/heads/feature").unwrap();
    let base = repo.revparse_single("main~1").unwrap().id();
    repo.reference("refs/heads/behind", base, false, "t").unwrap();
    write(&repo, "a.txt", "dirty\n");
    let out = merge_branch(&repo, "refs/heads/feature", "refs/heads/behind", None).unwrap();
    assert!(matches!(out, MergeOutcome::FastForward { .. }));
    assert_eq!(repo.refname_to_id("refs/heads/behind").unwrap(), feature_tip);
    assert_eq!(read(&repo, "a.txt"), "dirty\n");
    assert_eq!(repo.head().unwrap().name(), Some("refs/heads/main"));
}

#[test]
fn fast_forward_refused_for_branch_checked_out_in_other_worktree() {
    let (tmp, repo) = diverged();
    let base = repo.revparse_single("main~1").unwrap().id();
    repo.reference("refs/heads/wt", base, false, "t").unwrap();
    let r = repo.find_reference("refs/heads/wt").unwrap();
    let mut opts = git2::WorktreeAddOptions::new();
    opts.reference(Some(&r));
    let wt_dir = TempDir::new().unwrap();
    repo.worktree("wt", &wt_dir.path().join("wt"), Some(&opts)).unwrap();
    let err = merge_branch(&repo, "refs/heads/feature", "refs/heads/wt", None).unwrap_err();
    assert!(matches!(&err, GitYasError::InvalidInput(m) if m.contains("worktree")), "{err:?}");
    // From inside the linked worktree it is the checked-out branch, so it fast-forwards there.
    let wt = repository::open(&wt_dir.path().join("wt")).unwrap();
    assert!(matches!(
        merge_branch(&wt, "refs/heads/feature", "refs/heads/wt", None).unwrap(),
        MergeOutcome::FastForward { .. }
    ));
    assert_eq!(read(&wt, "b.txt"), "feature\n");
    drop(tmp);
}

#[test]
fn normal_merge_creates_merge_commit() {
    let (tmp, repo) = diverged();
    let main_tip = head_oid(&repo);
    let feature_tip = repo.refname_to_id("refs/heads/feature").unwrap();
    write(&repo, "a.txt", "one\ntwo\nthree\nlocal\n"); // unrelated unstaged change is carried
    let out = merge_branch(&repo, "refs/heads/feature", "refs/heads/main", Some(&core_sig())).unwrap();
    let MergeOutcome::Merged { commit } = out else { panic!("{out:?}") };
    let c = repo.find_commit(git2::Oid::from_bytes(commit.as_bytes()).unwrap()).unwrap();
    assert_eq!(c.parent_ids().collect::<Vec<_>>(), [main_tip, feature_tip]);
    assert_eq!(c.summary(), Some("Merge branch 'feature' into main"));
    assert_eq!(c.author().name(), Some("Test User"));
    assert_eq!(head_oid(&repo), c.id());
    assert_eq!(repository::state(&repo), RepoState::Clean);
    assert_eq!(read(&repo, "b.txt"), "feature\n");
    assert_eq!(git(tmp.path(), &["status", "--porcelain"]), " M a.txt\n");
    // Oracle: the tree equals what `git merge-tree` computes.
    let expected = git(
        tmp.path(),
        &["merge-tree", "--write-tree", &main_tip.to_string(), &feature_tip.to_string()],
    );
    assert_eq!(expected.lines().next().unwrap(), c.tree_id().to_string());
}

#[test]
fn normal_merge_requires_signature_and_clean_index() {
    let (_tmp, repo) = diverged();
    let before = head_oid(&repo);
    for sig in [
        None,
        Some(gityas_core::models::CommitSignature {
            name: "".into(),
            email: "".into(),
        }),
    ] {
        let err = merge_branch(&repo, "refs/heads/feature", "refs/heads/main", sig.as_ref()).unwrap_err();
        assert!(matches!(&err, GitYasError::SignatureRequired(_)), "{err:?}");
    }
    write(&repo, "c.txt", "staged change\n");
    stage(&repo, "c.txt");
    let err = merge_branch(&repo, "refs/heads/feature", "refs/heads/main", Some(&core_sig())).unwrap_err();
    assert!(matches!(&err, GitYasError::InvalidInput(m) if m.contains("staged")), "{err:?}");
    assert_eq!(head_oid(&repo), before);
    assert_eq!(repository::state(&repo), RepoState::Clean);
}

#[test]
fn normal_merge_into_branch_not_checked_out_is_refused() {
    let (_tmp, repo) = diverged();
    branches::checkout_branch(&repo, "refs/heads/feature").unwrap();
    let err = merge_branch(&repo, "refs/heads/feature", "refs/heads/main", Some(&core_sig())).unwrap_err();
    assert!(matches!(&err, GitYasError::InvalidInput(m) if m.contains("check out")), "{err:?}");
    assert!(matches!(
        merge_branch(&repo, "refs/heads/main", "refs/remotes/x", Some(&core_sig())),
        Err(GitYasError::InvalidInput(_))
    ));
}

#[test]
fn merge_blocked_by_local_change_to_merged_file() {
    let (_tmp, repo) = diverged();
    // feature adds b.txt; an untracked local b.txt would be overwritten.
    write(&repo, "b.txt", "mine\n");
    let before = head_oid(&repo);
    let err = merge_branch(&repo, "refs/heads/feature", "refs/heads/main", Some(&core_sig())).unwrap_err();
    assert!(
        matches!(&err, GitYasError::Conflict(p) if p.iter().any(|p| p == std::path::Path::new("b.txt"))),
        "{err:?}"
    );
    assert_eq!(read(&repo, "b.txt"), "mine\n");
    assert_eq!(head_oid(&repo), before);
    assert_eq!(
        repository::state(&repo),
        RepoState::Clean,
        "refused merge must not leave MERGE_HEAD"
    );
}

#[test]
fn conflicts_are_left_for_the_conflict_ui_and_abort_restores() {
    let (tmp, repo) = conflicting();
    let before = head_oid(&repo);
    write(&repo, "unrelated-untracked.txt", "keep me\n");
    let out = merge_branch(&repo, "refs/heads/feature", "refs/heads/main", Some(&core_sig())).unwrap();
    assert_eq!(
        out,
        MergeOutcome::Conflicts {
            paths: vec![PathBuf::from("a.txt")]
        }
    );
    assert_eq!(repository::state(&repo), RepoState::Merge);
    assert!(read(&repo, "a.txt").contains("<<<<<<<"));
    assert_eq!(read(&repo, "added-by-feature.txt"), "x\n");
    let status = git(tmp.path(), &["status", "--porcelain"]);
    assert!(
        status.contains("UU a.txt") && status.contains("A  added-by-feature.txt"),
        "{status}"
    );

    // Merging again or checking out while merging is refused.
    assert!(matches!(
        merge_branch(&repo, "refs/heads/feature", "refs/heads/main", Some(&core_sig())),
        Err(GitYasError::InvalidInput(m)) if m.contains("merge")
    ));

    let aborted = abort_merge(&repo).unwrap();
    assert_eq!(aborted.restored, [PathBuf::from("a.txt")]);
    assert_eq!(aborted.removed, [PathBuf::from("added-by-feature.txt")]);
    assert_eq!(repository::state(&repo), RepoState::Clean);
    assert_eq!(head_oid(&repo), before);
    assert_eq!(read(&repo, "a.txt"), "one\nMAIN\nthree\n");
    assert!(!tmp.path().join("added-by-feature.txt").exists());
    assert_eq!(read(&repo, "unrelated-untracked.txt"), "keep me\n");
    assert_eq!(git(tmp.path(), &["status", "--porcelain"]), "?? unrelated-untracked.txt\n");
    // Nothing to abort now.
    assert!(matches!(abort_merge(&repo), Err(GitYasError::InvalidInput(_))));
}

#[test]
fn abort_keeps_unrelated_unstaged_changes() {
    let (tmp, repo) = conflicting();
    commit_file(&repo, "keep.txt", "v1\n", "add keep");
    write(&repo, "keep.txt", "local work\n");
    merge_branch(&repo, "refs/heads/feature", "refs/heads/main", Some(&core_sig())).unwrap();
    abort_merge(&repo).unwrap();
    assert_eq!(read(&repo, "keep.txt"), "local work\n", "old abort did reset --hard and lost this");
    assert_eq!(git(tmp.path(), &["status", "--porcelain"]), " M keep.txt\n");
}

/// A conflicted path HEAD does not have (main deleted it, feature modified it): libgit2's `reset_default` turned it
/// into an index entry with mode 0, so the abort failed with "invalid entry mode".
#[test]
fn abort_modify_delete_conflict() {
    let tmp = TempDir::new().unwrap();
    let repo = init(tmp.path());
    commit_file(&repo, "a.txt", "a\n", "base a");
    commit_file(&repo, "dir/b.txt", "one\n", "base b");
    branches::create_branch(&repo, "feature", None, true).unwrap();
    commit_file(&repo, "dir/b.txt", "FEATURE\n", "feature edits b");
    branches::checkout_branch(&repo, "refs/heads/main").unwrap();
    std::fs::remove_file(tmp.path().join("dir/b.txt")).unwrap();
    stage(&repo, "dir/b.txt");
    commit_index(&repo, "main deletes b");
    let before = head_oid(&repo);

    let out = merge_branch(&repo, "refs/heads/feature", "refs/heads/main", Some(&core_sig())).unwrap();
    assert_eq!(
        out,
        MergeOutcome::Conflicts {
            paths: vec![PathBuf::from("dir/b.txt")]
        }
    );
    let aborted = abort_merge(&repo).unwrap();
    assert!(aborted.restored.is_empty());
    assert_eq!(aborted.removed, [PathBuf::from("dir/b.txt")]);
    assert_eq!(repository::state(&repo), RepoState::Clean);
    assert_eq!(head_oid(&repo), before);
    assert!(!tmp.path().join("dir/b.txt").exists());
    assert_eq!(git(tmp.path(), &["status", "--porcelain"]), "");
}

#[test]
fn merge_into_unborn_branch() {
    let tmp = TempDir::new().unwrap();
    let repo = init(tmp.path());
    let src = TempDir::new().unwrap();
    let source = init(src.path());
    let c = commit_file(&source, "s.txt", "s\n", "src");
    repo.remote("src", &file_url(src.path())).unwrap();
    repo.find_remote("src").unwrap().fetch::<&str>(&[], None, None).unwrap();
    let out = merge_branch(&repo, "refs/remotes/src/main", "refs/heads/main", None).unwrap();
    assert_eq!(out, MergeOutcome::FastForward { from: None, to: oid(c) });
    assert_eq!(head_oid(&repo), c);
    assert_eq!(read(&repo, "s.txt"), "s\n");
}

#[test]
fn merge_in_bare_repo_is_refused() {
    let fx = Fixture::new();
    assert!(matches!(
        merge_branch(&fx.remote(), "refs/heads/main", "refs/heads/main", None),
        Err(GitYasError::InvalidInput(m)) if m.contains("bare")
    ));
    assert!(abort_merge(&fx.remote()).is_err());
}

#[test]
fn merge_from_detached_head_into_branch() {
    let (_tmp, repo) = diverged();
    let main_tip = head_oid(&repo);
    repo.set_head_detached(main_tip).unwrap();
    // main isn't "checked out" (HEAD detached) and can't be fast-forwarded to feature: refused, not corrupted.
    let err = merge_branch(&repo, "refs/heads/feature", "refs/heads/main", Some(&core_sig())).unwrap_err();
    assert!(matches!(err, GitYasError::InvalidInput(_)));
    assert!(repo.head_detached().unwrap());
}
