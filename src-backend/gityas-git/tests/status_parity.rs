//! gitoxide status (`get_status`, `get_status_paths`) against the previous libgit2 implementation
//! (`get_status_libgit2`) on working trees with every kind of change at once.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod worktree_support;

use std::path::PathBuf;

use gityas_core::models::{Delta, DeltaChange, FileState};
use gityas_git::status::{get_status, get_status_libgit2, get_status_paths};
use worktree_support::TestRepo;

fn assert_parity(repo: &TestRepo) -> FileState {
    let git2 = repo.open();
    let want = get_status_libgit2(&git2).unwrap();
    let got = get_status(&git2).unwrap();
    assert_eq!(got.staged, want.staged, "staged");
    assert_eq!(got.unstaged, want.unstaged, "unstaged");
    got
}

/// For every unstaged path (and some unchanged and missing ones), the path-limited status agrees with the full one.
fn assert_paths_agree(repo: &TestRepo, full: &FileState, extra: &[&str]) {
    let git2 = repo.open();
    let mut paths: Vec<PathBuf> = full
        .unstaged
        .iter()
        .filter(|d| !matches!(d.change, DeltaChange::Renamed | DeltaChange::Copied))
        .map(|d| d.path.clone())
        .collect();
    paths.extend(extra.iter().map(PathBuf::from));
    for path in &paths {
        let want: Vec<Delta> = full.unstaged.iter().filter(|d| under(&d.path, path)).cloned().collect();
        let got = get_status_paths(&git2, std::slice::from_ref(path)).unwrap();
        assert_eq!(got, want, "path {}", path.display());
    }
    let all = get_status_paths(&git2, &paths).unwrap();
    let want: Vec<Delta> = full
        .unstaged
        .iter()
        .filter(|d| paths.iter().any(|p| under(&d.path, p)))
        .cloned()
        .collect();
    assert_eq!(all, want, "all paths at once");
}

fn under(path: &std::path::Path, spec: &std::path::Path) -> bool {
    let path = path.to_string_lossy().replace('\\', "/");
    let spec = spec.to_string_lossy().replace('\\', "/");
    let spec = spec.trim_end_matches('/');
    path == spec || path.trim_end_matches('/') == spec || path.starts_with(&format!("{spec}/"))
}

#[test]
fn mixed_working_tree_matches_libgit2() {
    let repo = TestRepo::init();
    repo.write(".gitignore", "*.log\nbuild/\n");
    repo.write("keep.txt", "keep\n");
    repo.write("modify.txt", "one\n");
    repo.write("delete.txt", "gone soon\n");
    repo.write("stage-modify.txt", "a\n");
    repo.write("stage-delete.txt", "b\n");
    repo.write("rename-me.txt", "line\n".repeat(30));
    repo.write("dir/nested/deep.txt", "deep\n");
    repo.write("both.txt", "base\n");
    repo.commit_all("base");

    // Staged: add, modify, delete, rename.
    repo.write("staged-new.txt", "new\n");
    repo.write("stage-modify.txt", "a2\n");
    repo.remove("stage-delete.txt");
    std::fs::rename(repo.path().join("rename-me.txt"), repo.path().join("renamed.txt")).unwrap();
    repo.git(&[
        "add",
        "-A",
        "staged-new.txt",
        "stage-modify.txt",
        "stage-delete.txt",
        "rename-me.txt",
        "renamed.txt",
    ]);
    // Unstaged: modify, delete; staged and then modified again.
    repo.write("modify.txt", "two\n");
    repo.remove("delete.txt");
    repo.write("both.txt", "staged\n");
    repo.git(&["add", "both.txt"]);
    repo.write("both.txt", "worktree\n");
    // Untracked: files, nested directories; ignored ones excluded.
    repo.write("untracked.txt", "u\n");
    repo.write("newdir/a.txt", "a\n");
    repo.write("newdir/sub/b.txt", "b\n");
    repo.write("debug.log", "ignored\n");
    repo.write("build/out.bin", "ignored\n");
    repo.write("dir/nested/new-in-tracked-dir.txt", "n\n");

    let full = assert_parity(&repo);
    assert!(full.staged.iter().any(|d| d.change == DeltaChange::Renamed), "{full:?}");
    assert!(full.unstaged.iter().any(|d| d.change == DeltaChange::Untracked));
    assert_paths_agree(
        &repo,
        &full,
        &["keep.txt", "newdir", "dir", "missing/never.txt", "debug.log", "build"],
    );
}

#[test]
fn conflicts_unborn_and_clean_match_libgit2() {
    let repo = TestRepo::init();
    repo.write("a.txt", "a\n");
    repo.git(&["add", "a.txt"]);
    repo.write("b.txt", "b\n");
    assert_parity(&repo); // unborn HEAD

    repo.write("c.txt", "base\n");
    repo.commit_all("base");
    let clean = assert_parity(&repo);
    assert!(clean.staged.is_empty() && clean.unstaged.is_empty(), "{clean:?}");

    repo.git(&["checkout", "-qb", "other"]);
    repo.write("c.txt", "theirs\n");
    repo.write("only-theirs.txt", "t\n");
    repo.commit_all("theirs");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("c.txt", "ours\n");
    repo.commit_all("ours");
    assert!(!repo.git_try(&["merge", "other"]).status.success());
    repo.write("unrelated.txt", "u\n");
    let full = assert_parity(&repo);
    assert!(full.unstaged.iter().any(|d| d.change == DeltaChange::Conflicted));
    assert_paths_agree(&repo, &full, &[]);
}

#[test]
fn line_endings_and_racy_files_match_libgit2() {
    let repo = TestRepo::init();
    repo.git(&["config", "core.autocrlf", "true"]);
    repo.write("crlf.txt", "one\r\ntwo\r\n");
    repo.write("lf.txt", "one\ntwo\n");
    repo.commit_all("base");
    // Same content, new timestamp: not a change. Different content, same size: a change.
    repo.write("crlf.txt", "one\r\ntwo\r\n");
    repo.write("lf.txt", "one\nTWO\n");
    let full = assert_parity(&repo);
    assert_paths_agree(&repo, &full, &["crlf.txt"]);
}

#[cfg(unix)]
#[test]
fn typechange_and_mode_match_libgit2() {
    use std::os::unix::fs::PermissionsExt;
    let repo = TestRepo::init();
    repo.write("script.sh", "echo\n");
    repo.write("link-me.txt", "x\n");
    repo.commit_all("base");
    std::fs::set_permissions(repo.path().join("script.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    repo.remove("link-me.txt");
    std::os::unix::fs::symlink("script.sh", repo.path().join("link-me.txt")).unwrap();
    let full = assert_parity(&repo);
    assert_paths_agree(&repo, &full, &[]);
}
