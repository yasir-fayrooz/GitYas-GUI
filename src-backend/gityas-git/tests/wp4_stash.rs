//! WP4: stash list / details / apply / pop / drop.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test crate: panics are the assertions

mod wp4_support;

use std::path::PathBuf;

use gityas_core::GitYasError;
use gityas_core::models::DeltaChange;
use gityas_git::stash::{self, StashApplyOutcome};
use tempfile::TempDir;
use wp4_support::*;

#[test]
fn empty_and_unborn() {
    let tmp = TempDir::new().unwrap();
    let mut repo = init(tmp.path());
    assert!(stash::list_stashes(&repo).unwrap().is_empty());
    assert!(matches!(stash::stash_details(&repo, 0), Err(GitYasError::NotFound(_))));
    assert!(matches!(stash::drop_stash(&mut repo, 0), Err(GitYasError::NotFound(_))));
    assert!(matches!(stash::apply_stash(&mut repo, 0, false), Err(GitYasError::NotFound(_))));
}

/// Base: a.txt, gone.txt committed. Stash: modify a.txt, delete gone.txt, add staged new.txt, untracked u.txt.
fn repo_with_stash() -> (TempDir, git2::Repository) {
    let tmp = TempDir::new().unwrap();
    let mut repo = init(tmp.path());
    commit_file(&repo, "a.txt", "one\n", "c1");
    commit_file(&repo, "gone.txt", "bye\n", "c2");
    write(&repo, "a.txt", "two\n");
    std::fs::remove_file(tmp.path().join("gone.txt")).unwrap();
    write(&repo, "new.txt", "new\n");
    stage(&repo, "new.txt");
    write(&repo, "u.txt", "untracked\n");
    repo.stash_save(&sig(), "my work", Some(git2::StashFlags::INCLUDE_UNTRACKED))
        .unwrap();
    (tmp, repo)
}

#[test]
fn list_counts_and_files() {
    let (tmp, mut repo) = repo_with_stash();
    write(&repo, "a.txt", "second\n");
    repo.stash_save(&sig(), "second", None).unwrap();

    let list = stash::list_stashes(&repo).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].stash.index, 0);
    assert!(list[0].stash.message.contains("second"), "{}", list[0].stash.message);
    let first = &list[1];
    assert!(first.stash.message.contains("my work"));
    assert_eq!(
        (first.stash.counts.added, first.stash.counts.modified, first.stash.counts.deleted),
        (2, 1, 1)
    );
    let change = |p: &str| first.files.iter().find(|d| d.path == std::path::Path::new(p)).map(|d| d.change);
    assert_eq!(change("a.txt"), Some(DeltaChange::Modified));
    assert_eq!(change("gone.txt"), Some(DeltaChange::Deleted));
    assert_eq!(change("new.txt"), Some(DeltaChange::Added));
    assert_eq!(change("u.txt"), Some(DeltaChange::Untracked));
    assert_eq!(first.stash.author.name, "Test User");
    assert_eq!(stash::stash_details(&repo, 1).unwrap(), first.clone());
    // Oracle: same count and order as `git stash list`.
    let cli = git(tmp.path(), &["stash", "list", "--format=%H"]);
    let ids: Vec<String> = list.iter().map(|s| s.stash.id.to_hex()).collect();
    assert_eq!(cli.lines().collect::<Vec<_>>(), ids);
}

#[test]
fn apply_pop_drop() {
    let (tmp, mut repo) = repo_with_stash();
    assert_eq!(stash::apply_stash(&mut repo, 0, false).unwrap(), StashApplyOutcome::Applied);
    assert_eq!(read(&repo, "a.txt"), "two\n");
    assert_eq!(read(&repo, "u.txt"), "untracked\n");
    assert!(!tmp.path().join("gone.txt").exists());
    assert_eq!(stash::list_stashes(&repo).unwrap().len(), 1, "apply keeps the stash");

    // Reset and pop.
    git(tmp.path(), &["reset", "--hard", "-q"]);
    git(tmp.path(), &["clean", "-fdq"]);
    assert_eq!(stash::pop_stash(&mut repo, 0).unwrap(), StashApplyOutcome::Applied);
    assert!(stash::list_stashes(&repo).unwrap().is_empty());
    assert_eq!(read(&repo, "a.txt"), "two\n");

    // Drop.
    repo.stash_save(&sig(), "again", Some(git2::StashFlags::INCLUDE_UNTRACKED)).unwrap();
    stash::drop_stash(&mut repo, 0).unwrap();
    assert!(stash::list_stashes(&repo).unwrap().is_empty());
    assert_eq!(git(tmp.path(), &["stash", "list"]), "");
}

#[test]
fn apply_refuses_to_overwrite_local_changes() {
    let (_tmp, mut repo) = repo_with_stash();
    write(&repo, "a.txt", "local edit\n");
    let err = stash::apply_stash(&mut repo, 0, false).unwrap_err();
    assert!(
        matches!(&err, GitYasError::Conflict(p) if p.iter().any(|p| p == std::path::Path::new("a.txt"))),
        "{err:?}"
    );
    assert_eq!(read(&repo, "a.txt"), "local edit\n");
    // Staged changes: libgit2 refuses; clear message.
    write(&repo, "other.txt", "x\n");
    stage(&repo, "other.txt");
    let err = stash::pop_stash(&mut repo, 0).unwrap_err();
    assert!(matches!(&err, GitYasError::InvalidInput(m) if m.contains("staged")), "{err:?}");
    assert_eq!(stash::list_stashes(&repo).unwrap().len(), 1, "failed pop must not drop");
}

#[test]
fn pop_with_conflicts_keeps_the_stash() {
    let tmp = TempDir::new().unwrap();
    let mut repo = init(tmp.path());
    commit_file(&repo, "a.txt", "base\n", "c1");
    write(&repo, "a.txt", "stashed\n");
    repo.stash_save(&sig(), "s", None).unwrap();
    commit_file(&repo, "a.txt", "committed\n", "c2");

    let out = stash::pop_stash(&mut repo, 0).unwrap();
    assert_eq!(
        out,
        StashApplyOutcome::Conflicts {
            paths: vec![PathBuf::from("a.txt")]
        }
    );
    assert_eq!(
        stash::list_stashes(&repo).unwrap().len(),
        1,
        "conflicting pop must keep the stash (libgit2 would drop it)"
    );
    assert!(read(&repo, "a.txt").contains("<<<<<<<"));
    assert!(git(tmp.path(), &["status", "--porcelain"]).starts_with("UU a.txt"));
}

#[cfg(unix)]
#[test]
fn non_utf8_stash_message_does_not_panic() {
    let (tmp, repo) = repo_with_stash();
    let log = tmp.path().join(".git/logs/refs/stash");
    let bytes = std::fs::read(&log).unwrap();
    let tab = bytes.iter().position(|&b| b == b'\t').unwrap();
    let mut rewritten = bytes[..=tab].to_vec();
    rewritten.extend_from_slice(b"On main: caf\xe9\n");
    std::fs::write(&log, rewritten).unwrap();
    let list = stash::list_stashes(&repo).unwrap();
    assert_eq!(list.len(), 1);
    assert!(list[0].stash.message.starts_with("On main: caf"));
}

#[test]
fn bare_repo_cannot_apply() {
    let tmp = TempDir::new().unwrap();
    let mut bare = gityas_git::repository::init(tmp.path(), true, None).unwrap();
    assert!(stash::list_stashes(&bare).unwrap().is_empty());
    assert!(matches!(stash::apply_stash(&mut bare, 0, false), Err(GitYasError::InvalidInput(_))));
}
