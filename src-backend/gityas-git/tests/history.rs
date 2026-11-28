//! One file's history (renames followed) and blame.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod repo_support;

use std::path::{Path, PathBuf};

use gityas_core::GitYasError;
use gityas_core::diff::DiffTarget;
use gityas_core::models::DeltaChange;
use gityas_git::history::{file_blame, file_history};
use repo_support::{commit_file, commit_index, init, oid, stage, write};

const BODY: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";

#[test]
fn history_lists_the_commits_that_changed_the_file_and_follows_a_rename() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    assert!(file_history(&repo, Path::new("a.txt"), 100).unwrap().is_empty(), "unborn HEAD");

    let added = commit_file(&repo, "old.txt", BODY, "add old");
    commit_file(&repo, "other.txt", "x\n", "unrelated");
    let edited = commit_file(&repo, "old.txt", &BODY.replace("two", "TWO"), "edit old");
    // Rename old.txt -> new.txt (content unchanged).
    std::fs::rename(dir.path().join("old.txt"), dir.path().join("new.txt")).unwrap();
    stage(&repo, "old.txt");
    stage(&repo, "new.txt");
    let renamed = commit_index(&repo, "rename");
    let after = commit_file(&repo, "new.txt", &BODY.replace("two", "TWO").replace("six", "SIX"), "edit new");

    let history = file_history(&repo, Path::new("new.txt"), 100).unwrap();
    let ids: Vec<_> = history.iter().map(|e| e.commit.id.clone()).collect();
    assert_eq!(ids, vec![oid(after), oid(renamed), oid(edited), oid(added)]);
    assert_eq!(history[0].file.change, DeltaChange::Modified);
    assert_eq!(history[1].file.change, DeltaChange::Renamed);
    assert_eq!(history[1].file.old_path.as_deref(), Some(Path::new("old.txt")));
    assert_eq!(history[2].file.path, PathBuf::from("old.txt"), "older commits name the old path");
    assert_eq!(history[3].file.change, DeltaChange::Added);

    let capped = file_history(&repo, Path::new("new.txt"), 2).unwrap();
    assert_eq!(capped.len(), 2);
}

#[test]
fn blame_attributes_each_line_and_marks_uncommitted_ones() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    let first = commit_file(&repo, "f.txt", BODY, "first");
    let second = commit_file(&repo, "f.txt", &BODY.replace("three", "THREE"), "second");

    let at_commit = file_blame(&repo, &DiffTarget::Commit(oid(second)), Path::new("f.txt")).unwrap();
    assert_eq!(at_commit.text.lines().count(), 8);
    let covered: usize = at_commit.hunks.iter().map(|h| h.lines).sum();
    assert_eq!(covered, 8, "{:?}", at_commit.hunks);
    assert_eq!(at_commit.hunk_at(2).and_then(|h| h.commit.clone()), Some(oid(second)));
    assert_eq!(at_commit.hunk_at(2).map(|h| h.summary.as_str()), Some("second"));
    assert_eq!(at_commit.hunk_at(0).and_then(|h| h.commit.clone()), Some(oid(first)));

    // As of the first commit, every line is the first commit's.
    let older = file_blame(&repo, &DiffTarget::Commit(oid(first)), Path::new("f.txt")).unwrap();
    assert!(older.hunks.iter().all(|h| h.commit == Some(oid(first))));

    // Working tree: an edited line is not committed yet.
    write(&repo, "f.txt", &BODY.replace("three", "THREE").replace("seven", "7"));
    let worktree = file_blame(&repo, &DiffTarget::WorkingTree { staged: false }, Path::new("f.txt")).unwrap();
    assert_eq!(worktree.hunk_at(6).map(|h| h.is_committed()), Some(false));
    assert_eq!(worktree.hunk_at(2).and_then(|h| h.commit.clone()), Some(oid(second)));

    // A new file: all uncommitted.
    write(&repo, "new.txt", "a\nb\n");
    let new = file_blame(&repo, &DiffTarget::WorkingTree { staged: false }, Path::new("new.txt")).unwrap();
    assert_eq!(new.hunks.len(), 1);
    assert!(!new.hunks[0].is_committed() && new.hunks[0].lines == 2);

    // Binary content and missing paths are errors.
    std::fs::write(dir.path().join("bin.dat"), [0u8, 1, 2]).unwrap();
    assert!(matches!(
        file_blame(&repo, &DiffTarget::WorkingTree { staged: false }, Path::new("bin.dat")),
        Err(GitYasError::InvalidInput(_))
    ));
    assert!(matches!(
        file_blame(&repo, &DiffTarget::Commit(oid(first)), Path::new("nope.txt")),
        Err(GitYasError::NotFound(_))
    ));
}
