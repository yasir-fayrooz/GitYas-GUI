//! WP5 stash creation: path-limited `stash_files` (oracle: `git stash push -- <paths>`), `create_stash`, `pop_stash`,
//! and the libgit2 defect that rules out `stash_save_ext` + pathspec.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test crate: helpers outside #[test] fns too

mod wp5_support;

use gityas_core::GitYasError;
use gityas_core::models::{CommitSignature, DeltaChange};
use gityas_git::stash::pop_stash;
use gityas_git::worktree::{create_stash, stash_files};
use wp5_support::{TestRepo, delta, renamed, sig};

/// Repository with a mix of staged, unstaged, deleted, new and untracked changes.
fn mixed() -> TestRepo {
    let repo = TestRepo::init();
    repo.write("a.txt", "a\n");
    repo.write("b.txt", "b\n");
    repo.write("d.txt", "d\n");
    repo.write("keep.txt", "keep\n");
    repo.commit_all("base");
    repo.write("a.txt", "a2\n");
    repo.write("b.txt", "b2\n");
    repo.git(&["add", "b.txt"]);
    repo.write("b.txt", "b3\n");
    repo.remove("d.txt");
    repo.write("n.txt", "n\n");
    repo.git(&["add", "n.txt"]);
    repo.write("keep.txt", "keep2\n");
    repo.write("u.txt", "u\n");
    repo.write("dir/v.txt", "v\n");
    repo
}

fn tree(repo: &TestRepo, rev: &str) -> String {
    repo.git(&["ls-tree", "-r", rev])
}

fn assert_same_stash(ours: &TestRepo, oracle: &TestRepo, with_untracked: bool) {
    assert_eq!(tree(ours, "stash@{0}"), tree(oracle, "stash@{0}"), "w tree");
    assert_eq!(tree(ours, "stash@{0}^2"), tree(oracle, "stash@{0}^2"), "i tree");
    if with_untracked {
        assert_eq!(tree(ours, "stash@{0}^3"), tree(oracle, "stash@{0}^3"), "u tree");
    } else {
        assert!(!ours.git_try(&["rev-parse", "-q", "--verify", "stash@{0}^3"]).status.success());
    }
    assert_eq!(ours.porcelain(), oracle.porcelain(), "remaining changes");
    for f in ["a.txt", "b.txt", "keep.txt"] {
        assert_eq!(ours.read(f), oracle.read(f), "{f}");
    }
    let subject = |r: &TestRepo| r.git(&["log", "-g", "--format=%gs", "refs/stash"]);
    assert_eq!(subject(ours), subject(oracle), "reflog message");
    assert_eq!(
        ours.git(&["log", "-1", "--format=%s", "stash@{0}^2"]),
        oracle.git(&["log", "-1", "--format=%s", "stash@{0}^2"])
    );
}

#[test]
fn stash_files_matches_git_stash_push_with_paths() {
    let ours = mixed();
    let oracle = ours.duplicate();
    let mut r = ours.open();
    stash_files(
        &mut r,
        &sig(),
        &[
            delta("a.txt", DeltaChange::Modified),
            delta("b.txt", DeltaChange::Modified),
            delta("d.txt", DeltaChange::Deleted),
            delta("n.txt", DeltaChange::Added),
        ],
        Some("my stash"),
        false,
    )
    .unwrap();
    oracle.git(&["stash", "push", "-q", "-m", "my stash", "--", "a.txt", "b.txt", "d.txt", "n.txt"]);
    assert_same_stash(&ours, &oracle, false);
    // Unselected changes are untouched.
    assert_eq!(ours.read("keep.txt"), b"keep2\n");
    assert!(ours.path().join("u.txt").exists());
}

#[test]
fn stash_files_with_untracked_matches_git_stash_push_u() {
    let ours = mixed();
    let oracle = ours.duplicate();
    let mut r = ours.open();
    stash_files(
        &mut r,
        &sig(),
        &[delta("a.txt", DeltaChange::Modified), delta("u.txt", DeltaChange::Untracked)],
        Some("with untracked"),
        true,
    )
    .unwrap();
    oracle.git(&["stash", "push", "-q", "-u", "-m", "with untracked", "--", "a.txt", "u.txt"]);
    assert_same_stash(&ours, &oracle, true);
    assert!(!ours.path().join("u.txt").exists());
    assert!(ours.path().join("dir/v.txt").exists(), "unselected untracked file survives");
}

#[test]
fn stash_files_default_message_matches_git() {
    let ours = mixed();
    let oracle = ours.duplicate();
    let mut r = ours.open();
    stash_files(&mut r, &sig(), &[delta("keep.txt", DeltaChange::Modified)], None, false).unwrap();
    oracle.git(&["stash", "push", "-q", "--", "keep.txt"]);
    assert_same_stash(&ours, &oracle, false);
}

#[test]
fn stash_files_then_git_stash_pop_restores_selection() {
    let repo = mixed();
    let before = repo.porcelain();
    let mut r = repo.open();
    stash_files(
        &mut r,
        &sig(),
        &[delta("a.txt", DeltaChange::Modified), delta("u.txt", DeltaChange::Untracked)],
        None,
        true,
    )
    .unwrap();
    repo.git(&["stash", "pop", "-q"]);
    assert_eq!(repo.porcelain(), before);
}

#[test]
fn stash_files_rename_includes_both_paths() {
    let repo = TestRepo::with_commit();
    repo.git(&["mv", "a.txt", "moved.txt"]);
    let mut r = repo.open();
    stash_files(&mut r, &sig(), &[renamed("a.txt", "moved.txt")], None, false).unwrap();
    assert!(repo.porcelain().is_empty(), "{:?}", repo.porcelain());
    repo.git(&["stash", "pop", "-q", "--index"]);
    assert_eq!(repo.porcelain(), vec!["R  moved.txt <- a.txt"]);
}

#[test]
fn stash_files_errors() {
    let repo = mixed();
    let mut r = repo.open();
    let none = stash_files(&mut r, &sig(), &[], None, false);
    assert!(matches!(none, Err(GitYasError::InvalidInput(_))));
    // untracked only, but include_untracked = false: nothing to stash
    let untracked = stash_files(&mut r, &sig(), &[delta("u.txt", DeltaChange::Untracked)], None, false);
    assert!(matches!(untracked, Err(GitYasError::InvalidInput(_))), "{untracked:?}");
    let bad_sig = CommitSignature {
        name: String::new(),
        email: String::new(),
    };
    let sig_err = stash_files(&mut r, &bad_sig, &[delta("a.txt", DeltaChange::Modified)], None, false);
    assert!(matches!(sig_err, Err(GitYasError::SignatureRequired(_))));
    let escape = stash_files(&mut r, &sig(), &[delta("../x", DeltaChange::Modified)], None, false);
    assert!(matches!(escape, Err(GitYasError::InvalidInput(_))));
    assert!(
        !repo.git_try(&["rev-parse", "-q", "--verify", "refs/stash"]).status.success(),
        "no stash created"
    );

    let fresh = TestRepo::init();
    fresh.write("x", "x");
    let unborn = stash_files(&mut fresh.open(), &sig(), &[delta("x", DeltaChange::Untracked)], None, true);
    assert!(matches!(unborn, Err(GitYasError::InvalidInput(_))));
}

#[test]
#[cfg_attr(windows, ignore = "`*` is not allowed in Windows file names")]
fn stash_paths_are_literal() {
    let repo = TestRepo::init();
    repo.write("a*", "1\n");
    repo.write("ab", "1\n");
    repo.commit_all("base");
    repo.write("a*", "2\n");
    repo.write("ab", "2\n");
    let mut r = repo.open();
    stash_files(&mut r, &sig(), &[delta("a*", DeltaChange::Modified)], None, false).unwrap();
    assert_eq!(repo.porcelain(), vec![" M ab"]);
}

/// Why `stash_save_ext` + `StashSaveOptions::pathspec` is not used: libgit2 1.9 resets the whole working tree.
/// If this test starts failing, libgit2 fixed it and the plumbing in `worktree/stash.rs` could be revisited.
#[test]
fn libgit2_pathspec_stash_destroys_unselected_changes() {
    let repo = mixed();
    let mut r = repo.open();
    let signature = git2::Signature::now("t", "t@example.com").unwrap();
    let mut opts = git2::StashSaveOptions::new(signature);
    opts.pathspec("a.txt");
    r.stash_save_ext(Some(&mut opts)).unwrap();
    assert_eq!(repo.read("keep.txt"), b"keep\n", "libgit2 reverted an unselected file");
}

#[test]
fn create_stash_and_pop_round_trip() {
    let repo = mixed();
    let before = repo.porcelain();
    let mut r = repo.open();
    let id = create_stash(&mut r, &sig(), Some("auto"), true).unwrap();
    assert!(id.is_some());
    assert!(repo.porcelain().is_empty());
    assert_eq!(repo.git(&["log", "-g", "--format=%gs", "refs/stash"]).trim(), "On main: auto");
    pop_stash(&mut r, 0).unwrap();
    // libgit2 (like `git stash pop` without --index) does not restore the staged/unstaged split.
    let paths = |lines: &[String]| {
        let mut v: Vec<String> = lines.iter().map(|l| l[3..].to_owned()).collect();
        v.sort();
        v
    };
    let (restored, expected) = (paths(&repo.porcelain()), paths(&before));
    assert_eq!(restored, expected);
    assert_eq!(repo.read("a.txt"), b"a2\n");
    assert!(
        !repo.git_try(&["rev-parse", "-q", "--verify", "refs/stash"]).status.success(),
        "dropped"
    );
}

#[test]
fn create_stash_nothing_to_stash_is_none() {
    let repo = TestRepo::with_commit();
    assert_eq!(create_stash(&mut repo.open(), &sig(), None, true).unwrap(), None);
    let fresh = TestRepo::init();
    fresh.write("x", "x");
    assert!(matches!(
        create_stash(&mut fresh.open(), &sig(), None, true),
        Err(GitYasError::InvalidInput(_))
    ));
}

#[test]
fn pop_stash_errors() {
    let repo = TestRepo::with_commit();
    let mut r = repo.open();
    assert!(matches!(pop_stash(&mut r, 0), Err(GitYasError::NotFound(_))));
    repo.write("a.txt", "stashed\n");
    create_stash(&mut r, &sig(), None, false).unwrap();
    repo.write("a.txt", "conflicting local edit\n");
    let err = pop_stash(&mut r, 0).unwrap_err();
    assert!(
        matches!(&err, GitYasError::Conflict(p) if p == &[std::path::PathBuf::from("a.txt")]),
        "{err:?}"
    );
    assert_eq!(repo.read("a.txt"), b"conflicting local edit\n", "local change untouched");
    assert!(
        repo.git_try(&["rev-parse", "-q", "--verify", "refs/stash"]).status.success(),
        "stash kept"
    );
}
