//! Fields added to the core contracts later: `FileDiff::{old_mode, new_mode}` and `Commit::lossy`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test crate: helpers outside #[test] fns too

use std::fs;
use std::path::Path;

use git2::{ObjectType, Repository, Signature};
use gityas_core::diff::DiffTarget;
use gityas_core::models::{Delta, DeltaChange};
use gityas_git::commits::commit_row;
use gityas_git::diff::{DiffConfig, get_file_diff};

fn repo_with(file: &str, content: &str) -> (tempfile::TempDir, Repository, git2::Oid) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    fs::write(dir.path().join(file), content).unwrap();
    let commit = {
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(file)).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = Signature::now("T", "t@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap()
    };
    (dir, repo, commit)
}

fn unstaged(repo: &Repository, path: &str, change: DeltaChange) -> gityas_core::diff::FileDiff {
    get_file_diff(
        repo,
        &DiffTarget::WorkingTree { staged: false },
        &Delta::new(path, change),
        &DiffConfig::default(),
    )
    .unwrap()
}

#[cfg(unix)]
#[test]
fn mode_only_change_reports_both_modes() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, repo, _) = repo_with("run.sh", "echo hi\n");
    let full = dir.path().join("run.sh");
    let mut perms = fs::metadata(&full).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&full, perms).unwrap();

    let d = unstaged(&repo, "run.sh", DeltaChange::Modified);
    assert!(d.hunks.is_empty(), "no text change");
    assert_eq!((d.old_mode, d.new_mode), (Some(0o100_644), Some(0o100_755)));
    assert!(d.mode_changed());
}

#[test]
fn added_and_modified_sides_have_modes() {
    let (dir, repo, _) = repo_with("a.txt", "a\n");
    fs::write(dir.path().join("a.txt"), "b\n").unwrap();
    let modified = unstaged(&repo, "a.txt", DeltaChange::Modified);
    assert_eq!((modified.old_mode, modified.new_mode), (Some(0o100_644), Some(0o100_644)));
    assert!(!modified.mode_changed());

    fs::write(dir.path().join("new.txt"), "n\n").unwrap();
    let added = unstaged(&repo, "new.txt", DeltaChange::Untracked);
    assert_eq!((added.old_mode, added.new_mode), (None, Some(0o100_644)));
}

#[test]
fn commit_row_flags_non_utf8_author_or_summary() {
    let (_dir, repo, head) = repo_with("a.txt", "a\n");
    let ok = commit_row(&repo.find_commit(head).unwrap()).unwrap();
    assert!(!ok.lossy);

    // git2's Signature API only takes &str, so write a Latin-1 commit object by hand.
    let tree = repo.find_commit(head).unwrap().tree_id();
    let mut raw = format!("tree {tree}\nparent {head}\n").into_bytes();
    raw.extend_from_slice(b"author Jos\xe9 <j@example.com> 1700000000 +0000\n");
    raw.extend_from_slice(b"committer Jos\xe9 <j@example.com> 1700000000 +0000\n\ncaf\xe9\n");
    let oid = repo.odb().unwrap().write(ObjectType::Commit, &raw).unwrap();
    let row = commit_row(&repo.find_commit(oid).unwrap()).unwrap();
    assert!(row.lossy);
    assert_eq!(row.author_name, "Jos\u{fffd}");
    assert_eq!(row.summary, "caf\u{fffd}");
    assert_eq!(row.author_time, 1_700_000_000);
}
