//! Wave 1 integration: hunks produced by WP6's `get_file_diff` must be accepted by WP5's hunk operations
//! (same diff options, same `HunkRef::content_hash`). Each WP tested its half alone; this is the join.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test crate: helpers outside #[test] fns too

use std::fs;
use std::path::Path;

use git2::{Repository, Signature};
use gityas_core::GitYasError;
use gityas_core::diff::{DiffTarget, FileDiff, HunkRef};
use gityas_core::models::{Delta, FileState};
use gityas_git::diff::{DiffConfig, get_file_diff};
use gityas_git::status::get_status;
use gityas_git::worktree::{discard_hunk, stage_hunk, unstage_hunk};

const BASE: &str = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\n";

fn repo_with(file: &str, content: &[u8]) -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    // Byte-exact assertions: override Git for Windows' system `core.autocrlf=true`, which libgit2 also reads.
    repo.config().unwrap().set_bool("core.autocrlf", false).unwrap();
    fs::write(dir.path().join(file), content).unwrap();
    {
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(file)).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = Signature::now("T", "t@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
    }
    (dir, repo)
}

fn status(repo: &Repository) -> FileState {
    get_status(repo).unwrap()
}

fn delta<'a>(list: &'a [Delta], path: &str) -> &'a Delta {
    list.iter()
        .find(|d| d.path == Path::new(path))
        .unwrap_or_else(|| panic!("{path} not in {list:?}"))
}

fn diff(repo: &Repository, staged: bool, d: &Delta) -> FileDiff {
    get_file_diff(repo, &DiffTarget::WorkingTree { staged }, d, &DiffConfig::default()).unwrap()
}

fn index_content(repo: &Repository, path: &str) -> Vec<u8> {
    let index = repo.index().unwrap();
    let entry = index.get_path(Path::new(path), 0).unwrap();
    repo.find_blob(entry.id).unwrap().content().to_vec()
}

fn hunk_ids(d: &FileDiff) -> Vec<HunkRef> {
    d.hunks.iter().map(|h| h.id).collect()
}

#[test]
fn stage_unstage_discard_round_trip_with_two_hunks() {
    let (dir, repo) = repo_with("f.txt", BASE.as_bytes());
    let edited = BASE.replace("l2\n", "L2\n").replace("l18\n", "L18\n");
    fs::write(dir.path().join("f.txt"), &edited).unwrap();

    let st = status(&repo);
    let unstaged = diff(&repo, false, delta(&st.unstaged, "f.txt"));
    let hunks = hunk_ids(&unstaged);
    assert_eq!(hunks.len(), 2, "{unstaged:?}");

    // Stage the first hunk only.
    stage_hunk(&repo, delta(&st.unstaged, "f.txt"), &hunks[0]).unwrap();
    assert_eq!(index_content(&repo, "f.txt"), BASE.replace("l2\n", "L2\n").as_bytes());
    let st = status(&repo);
    assert_eq!(diff(&repo, true, delta(&st.staged, "f.txt")).hunks.len(), 1);
    let unstaged = diff(&repo, false, delta(&st.unstaged, "f.txt"));
    assert_eq!(unstaged.hunks.len(), 1);

    // Discard the remaining (second) hunk from the working tree.
    discard_hunk(&repo, delta(&st.unstaged, "f.txt"), &unstaged.hunks[0].id).unwrap();
    assert_eq!(fs::read_to_string(dir.path().join("f.txt")).unwrap(), BASE.replace("l2\n", "L2\n"));

    // Unstage the staged hunk using the staged diff's HunkRef.
    let st = status(&repo);
    let staged = diff(&repo, true, delta(&st.staged, "f.txt"));
    unstage_hunk(&repo, delta(&st.staged, "f.txt"), &staged.hunks[0].id).unwrap();
    assert_eq!(index_content(&repo, "f.txt"), BASE.as_bytes());
    let st = status(&repo);
    assert!(st.staged.is_empty(), "{st:?}");
    assert_eq!(st.unstaged.len(), 1);
}

#[test]
fn untracked_file_single_hunk_stages() {
    let (dir, repo) = repo_with("f.txt", BASE.as_bytes());
    fs::write(dir.path().join("new.txt"), "a\nb\n").unwrap();
    let st = status(&repo);
    let d = delta(&st.unstaged, "new.txt");
    let fd = diff(&repo, false, d);
    assert_eq!(fd.hunks.len(), 1);
    stage_hunk(&repo, d, &fd.hunks[0].id).unwrap();
    assert_eq!(index_content(&repo, "new.txt"), b"a\nb\n");
}

#[test]
fn missing_trailing_newline_and_crlf_hunks_stage() {
    let (dir, repo) = repo_with("eof.txt", b"a\nb\nc");
    fs::write(dir.path().join("eof.txt"), b"a\nb\nC").unwrap();
    let st = status(&repo);
    let d = delta(&st.unstaged, "eof.txt");
    let fd = diff(&repo, false, d);
    stage_hunk(&repo, d, &fd.hunks[0].id).unwrap();
    assert_eq!(index_content(&repo, "eof.txt"), b"a\nb\nC");

    let (dir, repo) = repo_with("crlf.txt", b"a\r\nb\r\nc\r\n");
    fs::write(dir.path().join("crlf.txt"), b"a\r\nB\r\nc\r\n").unwrap();
    let st = status(&repo);
    let d = delta(&st.unstaged, "crlf.txt");
    let fd = diff(&repo, false, d);
    stage_hunk(&repo, d, &fd.hunks[0].id).unwrap();
    assert_eq!(index_content(&repo, "crlf.txt"), b"a\r\nB\r\nc\r\n");
}

#[test]
fn hunk_from_stale_diff_is_rejected_and_nothing_changes() {
    let (dir, repo) = repo_with("f.txt", BASE.as_bytes());
    fs::write(dir.path().join("f.txt"), BASE.replace("l5\n", "L5\n")).unwrap();
    let st = status(&repo);
    let d = delta(&st.unstaged, "f.txt").clone();
    let shown = diff(&repo, false, &d);
    // The file changes after the diff was displayed: same ranges, different content.
    fs::write(dir.path().join("f.txt"), BASE.replace("l5\n", "X5\n")).unwrap();
    assert_eq!(stage_hunk(&repo, &d, &shown.hunks[0].id), Err(GitYasError::StaleHunk));
    assert_eq!(index_content(&repo, "f.txt"), BASE.as_bytes());
}
