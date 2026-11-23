//! Working-tree operations: file stage/unstage/discard and hunk stage/unstage/discard, with the git CLI as a
//! test-only oracle (`git add -A`, `git reset`, `git restore`, `git apply [--cached] [-R]`).
#![allow(clippy::unwrap_used, clippy::expect_used)] // test crate: helpers outside #[test] fns too

mod worktree_support;

use gityas_core::GitYasError;
use gityas_core::diff::HunkRef;
use gityas_core::models::DeltaChange;
use gityas_git::status::get_status;
use gityas_git::worktree::{discard_files, discard_hunk, stage_files, stage_hunk, unstage_files, unstage_hunk};
use worktree_support::{TestRepo, delta, hunk_refs, renamed, split_cli_diff};

/// 20 numbered lines, so edits far apart produce separate hunks.
fn numbered(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

fn edit_lines(n: usize, edits: &[(usize, &str)]) -> String {
    (1..=n)
        .map(|i| match edits.iter().find(|(l, _)| *l == i) {
            Some((_, text)) => format!("{text}\n"),
            None => format!("line {i}\n"),
        })
        .collect()
}

fn three_hunk_repo() -> TestRepo {
    let repo = TestRepo::init();
    repo.write("f.txt", numbered(40));
    repo.commit_all("base");
    repo.write("f.txt", edit_lines(40, &[(2, "CHANGED 2"), (20, "CHANGED 20"), (38, "CHANGED 38")]));
    repo
}

fn diffs(repo: &TestRepo) -> (String, String) {
    (repo.git(&["diff", "--cached"]), repo.git(&["diff"]))
}

// ---------------------------------------------------------------- file operations

#[test]
fn stage_files_matches_git_add() {
    let ours = TestRepo::with_commit();
    ours.write("b.txt", "b\n");
    ours.commit_all("b");
    ours.write("a.txt", "changed\n");
    ours.remove("b.txt");
    ours.write("dir/nested/new.txt", "new\n");
    let oracle = ours.duplicate();

    let r = ours.open();
    stage_files(
        &r,
        &[
            delta("a.txt", DeltaChange::Modified),
            delta("b.txt", DeltaChange::Deleted),
            delta("dir/nested/new.txt", DeltaChange::Untracked),
        ],
    )
    .unwrap();
    oracle.git(&["add", "-A", "--", "a.txt", "b.txt", "dir/nested/new.txt"]);
    assert_eq!(ours.porcelain(), oracle.porcelain());
    assert_eq!(diffs(&ours), diffs(&oracle));
}

#[test]
fn stage_files_rename_stages_both_paths() {
    let repo = TestRepo::with_commit();
    std::fs::rename(repo.path().join("a.txt"), repo.path().join("moved.txt")).unwrap();
    stage_files(&repo.open(), &[renamed("a.txt", "moved.txt")]).unwrap();
    assert_eq!(repo.porcelain(), vec!["R  moved.txt <- a.txt"]);
}

#[test]
fn stage_files_uses_current_state_not_stale_delta_kind() {
    // UI thinks the file is modified, but it was deleted meanwhile: stage the deletion, don't error.
    let repo = TestRepo::with_commit();
    repo.remove("a.txt");
    stage_files(&repo.open(), &[delta("a.txt", DeltaChange::Modified)]).unwrap();
    assert_eq!(repo.porcelain(), vec!["D  a.txt"]);
}

#[test]
fn unstage_files_matches_git_reset() {
    let ours = TestRepo::with_commit();
    ours.write("b.txt", "b\n");
    ours.commit_all("b");
    ours.write("a.txt", "changed\n");
    ours.remove("b.txt");
    ours.write("new.txt", "new\n");
    ours.git(&["add", "-A"]);
    let oracle = ours.duplicate();

    unstage_files(
        &ours.open(),
        &[
            delta("a.txt", DeltaChange::Modified),
            delta("b.txt", DeltaChange::Deleted),
            delta("new.txt", DeltaChange::Added),
        ],
    )
    .unwrap();
    oracle.git(&["reset", "-q", "--", "a.txt", "b.txt", "new.txt"]);
    assert_eq!(ours.porcelain(), oracle.porcelain());
}

#[test]
fn unstage_rename_restores_old_path() {
    let repo = TestRepo::with_commit();
    repo.git(&["mv", "a.txt", "moved.txt"]);
    unstage_files(&repo.open(), &[renamed("a.txt", "moved.txt")]).unwrap();
    assert_eq!(repo.porcelain(), vec![" D a.txt", "?? moved.txt"]);
}

#[test]
fn unstage_on_unborn_head_only_removes_given_paths() {
    // Old code ran `index.remove_all(["*"])` on an unborn HEAD: unstaging one file unstaged everything.
    let repo = TestRepo::init();
    repo.write("x.txt", "x\n");
    repo.write("y.txt", "y\n");
    repo.git(&["add", "-A"]);
    unstage_files(&repo.open(), &[delta("x.txt", DeltaChange::Added)]).unwrap();
    assert_eq!(repo.porcelain(), vec!["?? x.txt", "A  y.txt"]);
}

#[test]
#[cfg_attr(windows, ignore = "`*` is not allowed in Windows file names")]
fn unstage_paths_are_literal_not_globs() {
    let repo = TestRepo::with_commit();
    repo.write("star*.txt", "s\n");
    repo.write("starfish.txt", "f\n");
    repo.git(&["add", "-A"]);
    unstage_files(&repo.open(), &[delta("star*.txt", DeltaChange::Added)]).unwrap();
    assert_eq!(repo.porcelain(), vec!["?? star*.txt", "A  starfish.txt"]);
}

#[test]
fn discard_files_matches_git_restore_and_clean() {
    let ours = TestRepo::with_commit();
    ours.write("b.txt", "b\n");
    ours.commit_all("b");
    ours.write("a.txt", "staged\n");
    ours.git(&["add", "a.txt"]);
    ours.write("a.txt", "staged then edited\n");
    ours.remove("b.txt");
    ours.write("junk.txt", "junk\n");
    let oracle = ours.duplicate();

    discard_files(
        &ours.open(),
        &[
            delta("a.txt", DeltaChange::Modified),
            delta("b.txt", DeltaChange::Deleted),
            delta("junk.txt", DeltaChange::Untracked),
        ],
    )
    .unwrap();
    oracle.git(&["restore", "--", "a.txt", "b.txt"]);
    oracle.git(&["clean", "-fq", "--", "junk.txt"]);
    assert_eq!(ours.porcelain(), oracle.porcelain());
    assert_eq!(ours.read("a.txt"), b"staged\n", "staged content survives a discard");
    assert!(!ours.path().join("junk.txt").exists());
}

#[test]
fn discard_files_refuses_directories() {
    let repo = TestRepo::with_commit();
    std::fs::create_dir_all(repo.path().join("d/inner")).unwrap();
    repo.write("d/inner/f", "x");
    let err = discard_files(&repo.open(), &[delta("d", DeltaChange::Untracked)]).unwrap_err();
    assert!(matches!(err, GitYasError::InvalidInput(_)), "{err:?}");
    assert!(repo.path().join("d/inner/f").exists());
}

#[test]
fn file_ops_reject_escaping_paths() {
    let repo = TestRepo::with_commit();
    let r = repo.open();
    for bad in ["../outside", "/etc/passwd", "", "./a.txt"] {
        let d = delta(bad, DeltaChange::Modified);
        assert!(
            matches!(stage_files(&r, std::slice::from_ref(&d)), Err(GitYasError::InvalidInput(_))),
            "{bad}"
        );
        assert!(
            matches!(discard_files(&r, std::slice::from_ref(&d)), Err(GitYasError::InvalidInput(_))),
            "{bad}"
        );
    }
}

#[test]
fn file_ops_refuse_conflicted_paths() {
    let repo = conflicted_repo();
    let r = repo.open();
    let d = delta("c.txt", DeltaChange::Conflicted);
    for result in [
        stage_files(&r, std::slice::from_ref(&d)),
        unstage_files(&r, std::slice::from_ref(&d)),
        discard_files(&r, std::slice::from_ref(&d)),
    ] {
        assert!(matches!(result, Err(GitYasError::InvalidInput(_))), "{result:?}");
    }
    assert!(repo.porcelain().contains(&"UU c.txt".to_owned()));
}

pub fn conflicted_repo() -> TestRepo {
    let repo = TestRepo::init();
    repo.write("c.txt", "base\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-qb", "other"]);
    repo.write("c.txt", "theirs\n");
    repo.commit_all("theirs");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("c.txt", "ours\n");
    repo.commit_all("ours");
    let out = repo.git_try(&["merge", "other"]);
    assert!(!out.status.success());
    repo
}

// ---------------------------------------------------------------- hunk operations vs the CLI oracle

/// Applies hunk `k` of `git diff [--cached] -- f.txt` with `git apply <flags>` in the oracle repository.
fn oracle_apply(oracle: &TestRepo, cached_diff: bool, k: usize, flags: &[&str]) {
    let mut args = vec!["diff", "--no-ext-diff", "-U3"];
    if cached_diff {
        args.push("--cached");
    }
    args.extend(["--", "f.txt"]);
    let (header, hunks) = split_cli_diff(&oracle.git(&args));
    let patch = format!("{header}{}", hunks[k]);
    let patch_path = oracle.path().join(".git/oracle.patch");
    std::fs::write(&patch_path, patch).unwrap();
    let mut apply = vec!["apply", "--whitespace=nowarn"];
    apply.extend_from_slice(flags);
    let p = patch_path.to_str().unwrap().to_owned();
    apply.push(&p);
    oracle.git(&apply);
}

#[test]
fn stage_each_hunk_matches_git_apply_cached() {
    for k in 0..3 {
        let ours = three_hunk_repo();
        let oracle = ours.duplicate();
        let r = ours.open();
        let file = delta("f.txt", DeltaChange::Modified);
        let refs = hunk_refs(&r, &file, false);
        assert_eq!(refs.len(), 3);
        stage_hunk(&r, &file, &refs[k]).unwrap();
        oracle_apply(&oracle, false, k, &["--cached"]);
        assert_eq!(diffs(&ours), diffs(&oracle), "hunk {k}");
    }
}

#[test]
fn unstage_each_hunk_matches_git_apply_cached_reverse() {
    for k in 0..3 {
        let ours = three_hunk_repo();
        ours.git(&["add", "f.txt"]);
        let oracle = ours.duplicate();
        let r = ours.open();
        let file = delta("f.txt", DeltaChange::Modified);
        let refs = hunk_refs(&r, &file, true);
        assert_eq!(refs.len(), 3);
        unstage_hunk(&r, &file, &refs[k]).unwrap();
        oracle_apply(&oracle, true, k, &["--cached", "-R"]);
        assert_eq!(diffs(&ours), diffs(&oracle), "hunk {k}");
    }
}

#[test]
fn discard_each_hunk_matches_git_apply_reverse() {
    for k in 0..3 {
        let ours = three_hunk_repo();
        let oracle = ours.duplicate();
        let r = ours.open();
        let file = delta("f.txt", DeltaChange::Modified);
        let refs = hunk_refs(&r, &file, false);
        discard_hunk(&r, &file, &refs[k]).unwrap();
        oracle_apply(&oracle, false, k, &["-R"]);
        assert_eq!(diffs(&ours), diffs(&oracle), "hunk {k}");
        assert_eq!(ours.read("f.txt"), oracle.read("f.txt"));
    }
}

#[test]
fn hunks_applied_one_after_another_with_fresh_refs() {
    let repo = three_hunk_repo();
    let r = repo.open();
    let file = delta("f.txt", DeltaChange::Modified);
    // Stage the last, then the first: offsets of later hunks shift; fresh refs keep working.
    let refs = hunk_refs(&r, &file, false);
    stage_hunk(&r, &file, &refs[2]).unwrap();
    let refs = hunk_refs(&r, &file, false);
    assert_eq!(refs.len(), 2);
    stage_hunk(&r, &file, &refs[0]).unwrap();
    let refs = hunk_refs(&r, &file, false);
    stage_hunk(&r, &file, &refs[0]).unwrap();
    assert_eq!(repo.porcelain(), vec!["M  f.txt"]);
}

#[test]
fn hunk_with_added_and_removed_lines_and_no_trailing_newline() {
    let ours = TestRepo::init();
    ours.write("f.txt", "a\nb\nc");
    ours.commit_all("base");
    ours.write("f.txt", "a\nB\nc\nd");
    let oracle = ours.duplicate();
    let r = ours.open();
    let file = delta("f.txt", DeltaChange::Modified);
    let refs = hunk_refs(&r, &file, false);
    stage_hunk(&r, &file, &refs[0]).unwrap();
    oracle_apply(&oracle, false, 0, &["--cached"]);
    assert_eq!(diffs(&ours), diffs(&oracle));
    // and back out again
    let refs = hunk_refs(&r, &file, true);
    unstage_hunk(&r, &file, &refs[0]).unwrap();
    assert_eq!(ours.porcelain(), vec![" M f.txt"]);
}

#[test]
fn crlf_content_round_trips_through_hunk_staging() {
    let ours = TestRepo::init();
    ours.write("f.txt", "a\r\nb\r\nc\r\n");
    ours.commit_all("base");
    ours.write("f.txt", "a\r\nB\r\nc\r\n");
    let oracle = ours.duplicate();
    let r = ours.open();
    let file = delta("f.txt", DeltaChange::Modified);
    stage_hunk(&r, &file, &hunk_refs(&r, &file, false)[0]).unwrap();
    oracle_apply(&oracle, false, 0, &["--cached"]);
    assert_eq!(diffs(&ours), diffs(&oracle));
}

#[test]
fn stage_hunk_of_untracked_file_stages_exactly_the_shown_content() {
    let repo = TestRepo::with_commit();
    repo.write("new.txt", "hello\nworld");
    let r = repo.open();
    let file = delta("new.txt", DeltaChange::Untracked);
    let refs = hunk_refs(&r, &file, false);
    assert_eq!(refs.len(), 1);
    stage_hunk(&r, &file, &refs[0]).unwrap();
    assert_eq!(repo.porcelain(), vec!["A  new.txt"]);
    assert_eq!(repo.git(&["show", ":new.txt"]), "hello\nworld");
}

/// Same libgit2 quirk as the diff test `untracked_file_below_a_stray_dot_git_dir_is_diffed`: the hunk must still be found.
#[test]
fn stage_hunk_of_untracked_file_below_a_stray_dot_git_dir() {
    let repo = TestRepo::with_commit();
    repo.write("app/keep.txt", "k\n");
    repo.git(&["add", "app/keep.txt"]);
    repo.git(&["commit", "-q", "-m", "app"]);
    std::fs::create_dir_all(repo.path().join("app/.git")).unwrap();
    repo.write("app/new.txt", "hello\n");
    let r = repo.open();
    let file = delta("app/new.txt", DeltaChange::Untracked);
    // `hunk_refs` diffs path-limited itself; take the hunk the diff view shows.
    let target = gityas_core::diff::DiffTarget::WorkingTree { staged: false };
    let shown = gityas_git::diff::get_file_diff(&r, &target, &file, &Default::default()).unwrap();
    assert_eq!(shown.hunks.len(), 1);
    stage_hunk(&r, &file, &shown.hunks[0].id).unwrap();
    assert_eq!(repo.git(&["show", ":app/new.txt"]), "hello\n");
}

#[cfg(unix)]
#[test]
fn stage_hunk_of_untracked_executable_keeps_mode() {
    use std::os::unix::fs::PermissionsExt;
    let repo = TestRepo::with_commit();
    repo.write("run.sh", "#!/bin/sh\necho hi\n");
    std::fs::set_permissions(repo.path().join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let r = repo.open();
    let file = delta("run.sh", DeltaChange::Untracked);
    stage_hunk(&r, &file, &hunk_refs(&r, &file, false)[0]).unwrap();
    assert!(repo.git(&["ls-files", "-s", "run.sh"]).starts_with("100755 "));
    assert_eq!(repo.porcelain(), vec!["A  run.sh"]);
}

#[test]
fn discard_hunk_of_untracked_file_deletes_it() {
    let repo = TestRepo::with_commit();
    repo.write("new.txt", "hello\n");
    let r = repo.open();
    let file = delta("new.txt", DeltaChange::Untracked);
    discard_hunk(&r, &file, &hunk_refs(&r, &file, false)[0]).unwrap();
    assert!(!repo.path().join("new.txt").exists());
}

#[test]
fn hunk_ops_on_deleted_file() {
    let repo = TestRepo::with_commit();
    repo.remove("a.txt");
    let r = repo.open();
    let file = delta("a.txt", DeltaChange::Deleted);
    // discard (restore the file) ...
    discard_hunk(&r, &file, &hunk_refs(&r, &file, false)[0]).unwrap();
    assert_eq!(repo.read("a.txt"), b"one\ntwo\nthree\n");
    assert!(repo.porcelain().is_empty());
    // ... stage the deletion ...
    repo.remove("a.txt");
    stage_hunk(&r, &file, &hunk_refs(&r, &file, false)[0]).unwrap();
    assert_eq!(repo.porcelain(), vec!["D  a.txt"]);
    // ... and unstage it again.
    unstage_hunk(&r, &file, &hunk_refs(&r, &file, true)[0]).unwrap();
    assert_eq!(repo.porcelain(), vec![" D a.txt"]);
}

#[test]
fn unstage_hunk_of_added_file_on_unborn_head() {
    let repo = TestRepo::init();
    repo.write("x.txt", "x\n");
    repo.git(&["add", "x.txt"]);
    let r = repo.open();
    let file = delta("x.txt", DeltaChange::Added);
    unstage_hunk(&r, &file, &hunk_refs(&r, &file, true)[0]).unwrap();
    assert_eq!(repo.porcelain(), vec!["?? x.txt"]);
}

#[test]
fn unstage_hunk_of_staged_rename() {
    let repo = TestRepo::init();
    repo.write("old.txt", numbered(30));
    repo.commit_all("base");
    repo.git(&["mv", "old.txt", "new.txt"]);
    repo.write("new.txt", edit_lines(30, &[(3, "X"), (27, "Y")]));
    repo.git(&["add", "new.txt"]);
    let r = repo.open();
    let file = renamed("old.txt", "new.txt");
    let refs = hunk_refs(&r, &file, true);
    assert_eq!(refs.len(), 2);
    unstage_hunk(&r, &file, &refs[0]).unwrap();
    let staged = repo.git(&["diff", "--cached", "-M", "--", "old.txt", "new.txt"]);
    assert!(staged.contains("rename from old.txt"), "{staged}");
    assert!(staged.contains("+Y") && !staged.contains("+X"), "{staged}");
    let unstaged = repo.git(&["diff", "--", "new.txt"]);
    assert!(unstaged.contains("+X") && !unstaged.contains("+Y"), "{unstaged}");
}

// ---------------------------------------------------------------- stale hunks

#[test]
fn bug8_stale_hunk_after_file_changed_is_rejected() {
    let repo = three_hunk_repo();
    let r = repo.open();
    let file = delta("f.txt", DeltaChange::Modified);
    let refs = hunk_refs(&r, &file, false);
    // Same ranges, different content: the old line-number matching would have staged the new content.
    repo.write(
        "f.txt",
        edit_lines(40, &[(2, "EDITED AGAIN"), (20, "CHANGED 20"), (38, "CHANGED 38")]),
    );
    let before = diffs(&repo);
    assert_eq!(stage_hunk(&r, &file, &refs[0]), Err(GitYasError::StaleHunk));
    assert_eq!(discard_hunk(&r, &file, &refs[0]), Err(GitYasError::StaleHunk));
    assert_eq!(diffs(&repo), before, "nothing changed");
    // The untouched hunks are still valid.
    stage_hunk(&r, &file, &refs[1]).unwrap();
}

#[test]
fn bug8_stale_hunk_wrong_ranges_or_hash() {
    let repo = three_hunk_repo();
    let r = repo.open();
    let file = delta("f.txt", DeltaChange::Modified);
    let good = hunk_refs(&r, &file, false)[0];
    let shifted = HunkRef {
        old_start: good.old_start + 1,
        ..good
    };
    let rehashed = HunkRef {
        content_hash: good.content_hash ^ 1,
        ..good
    };
    for bad in [shifted, rehashed] {
        assert_eq!(stage_hunk(&r, &file, &bad), Err(GitYasError::StaleHunk));
        assert_eq!(discard_hunk(&r, &file, &bad), Err(GitYasError::StaleHunk));
        assert_eq!(unstage_hunk(&r, &file, &bad), Err(GitYasError::StaleHunk));
    }
    assert_eq!(repo.porcelain(), vec![" M f.txt"]);
}

#[test]
fn bug8_staged_ref_used_for_unstaged_side_is_stale() {
    let repo = three_hunk_repo();
    repo.git(&["add", "f.txt"]);
    let r = repo.open();
    let file = delta("f.txt", DeltaChange::Modified);
    let staged_ref = hunk_refs(&r, &file, true)[0];
    assert_eq!(stage_hunk(&r, &file, &staged_ref), Err(GitYasError::StaleHunk));
    assert_eq!(discard_hunk(&r, &file, &staged_ref), Err(GitYasError::StaleHunk));
}

#[test]
fn bug8_file_reverted_entirely_is_stale() {
    let repo = three_hunk_repo();
    let r = repo.open();
    let file = delta("f.txt", DeltaChange::Modified);
    let refs = hunk_refs(&r, &file, false);
    repo.git(&["checkout", "--", "f.txt"]);
    assert_eq!(stage_hunk(&r, &file, &refs[0]), Err(GitYasError::StaleHunk));
}

#[test]
fn hunk_ops_refuse_conflicted_file() {
    let repo = conflicted_repo();
    let r = repo.open();
    let file = delta("c.txt", DeltaChange::Conflicted);
    let fake = HunkRef {
        old_start: 1,
        old_lines: 1,
        new_start: 1,
        new_lines: 1,
        content_hash: 0,
    };
    assert!(matches!(stage_hunk(&r, &file, &fake), Err(GitYasError::InvalidInput(_))));
}

#[test]
fn status_after_hunk_staging_lists_file_in_both_lists() {
    let repo = three_hunk_repo();
    let r = repo.open();
    let file = delta("f.txt", DeltaChange::Modified);
    stage_hunk(&r, &file, &hunk_refs(&r, &file, false)[1]).unwrap();
    let state = get_status(&r).unwrap();
    assert_eq!(state.staged, vec![delta("f.txt", DeltaChange::Modified)]);
    assert_eq!(state.unstaged, vec![delta("f.txt", DeltaChange::Modified)]);
}

#[test]
fn unstage_hunk_of_staged_rename_without_trailing_newline_keeps_rename_and_mode() {
    let repo = TestRepo::init();
    let base: String = numbered(30).trim_end().to_owned();
    repo.write("old.sh", &base);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(repo.path().join("old.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    repo.commit_all("base");
    repo.git(&["mv", "old.sh", "new.sh"]);
    repo.write("new.sh", format!("{}LAST", edit_lines(29, &[(2, "X")])));
    repo.git(&["add", "new.sh"]);
    let r = repo.open();
    let file = renamed("old.sh", "new.sh");
    let refs = hunk_refs(&r, &file, true);
    assert_eq!(refs.len(), 2);
    unstage_hunk(&r, &file, &refs[1]).unwrap();
    let staged = repo.git(&["diff", "--cached", "-M", "--", "old.sh", "new.sh"]);
    assert!(
        staged.contains("rename from old.sh") && staged.contains("+X") && !staged.contains("LAST"),
        "{staged}"
    );
    #[cfg(unix)]
    assert!(repo.git(&["ls-files", "-s", "new.sh"]).starts_with("100755 "));
}

#[test]
fn hunk_ops_on_unstaged_rename() {
    let repo = TestRepo::init();
    repo.write("old.txt", numbered(30));
    repo.commit_all("base");
    repo.remove("old.txt");
    repo.write("new.txt", edit_lines(30, &[(3, "X"), (27, "Y")]));
    let r = repo.open();
    let state = get_status(&r).unwrap();
    assert_eq!(state.unstaged, vec![renamed("old.txt", "new.txt")]);
    let file = renamed("old.txt", "new.txt");
    let refs = hunk_refs(&r, &file, false);
    assert_eq!(refs.len(), 2);
    // Discard one hunk: only new.txt's content changes; old.txt stays deleted, new.txt untracked.
    discard_hunk(&r, &file, &refs[0]).unwrap();
    assert_eq!(repo.read("new.txt"), edit_lines(30, &[(27, "Y")]).as_bytes());
    assert_eq!(repo.porcelain(), vec![" D old.txt", "?? new.txt"]);
    // Stage the remaining hunk: stages the rename with that content.
    let refs = hunk_refs(&r, &file, false);
    stage_hunk(&r, &file, &refs[0]).unwrap();
    assert_eq!(repo.porcelain(), vec!["R  new.txt <- old.txt"]);
}
