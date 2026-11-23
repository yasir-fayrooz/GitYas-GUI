//! WP5 repository-state matrix (review checklist): empty/unborn, detached HEAD, no remotes, bare, linked worktree,
//! submodule, shallow clone, merge in progress, non-UTF-8 content and paths, very large file / long history.
//! Each state runs the WP5 API surface that applies to it.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test crate: helpers outside #[test] fns too

mod wp5_support;

use git2::Repository;
use gityas_core::GitYasError;
use gityas_core::models::{DeltaChange, Head};
use gityas_git::cache::{ref_tips, should_bypass_cache, walk_new_commits};
use gityas_git::commits::{commit_staged_files, get_commit_files_list, get_commits_live, get_head};
use gityas_git::status::get_status;
use gityas_git::worktree::{discard_files, stage_files, stage_hunk, stash_files, unstage_files};
use wp5_support::{TestRepo, delta, file_url, git_ok, hunk_refs, sig};

/// Common smoke run: status, head, live walk, ref tips, stage + unstage + commit of a new file.
fn exercise(repo: &TestRepo) {
    let mut r = repo.open();
    get_status(&r).unwrap();
    get_head(&r).unwrap();
    let before = get_commits_live(&r).unwrap();
    ref_tips(&r).unwrap();
    repo.write("matrix_new.txt", "new\n");
    let d = delta("matrix_new.txt", DeltaChange::Untracked);
    stage_files(&r, std::slice::from_ref(&d)).unwrap();
    unstage_files(&r, std::slice::from_ref(&d)).unwrap();
    stage_files(&r, std::slice::from_ref(&d)).unwrap();
    let id = commit_staged_files(&mut r, &sig(), "matrix commit", None).unwrap();
    let after = get_commits_live(&r).unwrap();
    assert_eq!(after.first().map(|c| &c.id), Some(&id));
    assert_eq!(after.len(), before.len() + 1);
    assert_eq!(
        get_commit_files_list(&r, &id).unwrap(),
        vec![delta("matrix_new.txt", DeltaChange::Added)]
    );
}

#[test]
fn state_empty_unborn() {
    let repo = TestRepo::init();
    let r = repo.open();
    assert!(get_status(&r).unwrap().staged.is_empty());
    assert!(matches!(get_head(&r).unwrap(), Head::Unborn { .. }));
    assert!(get_commits_live(&r).unwrap().is_empty());
    assert!(ref_tips(&r).unwrap().is_empty());
    assert!(walk_new_commits(&r, &[], &[]).unwrap().is_empty());
    assert!(!should_bypass_cache(&r).unwrap());
    exercise(&repo);
}

#[test]
fn state_detached_head() {
    let repo = TestRepo::with_commit();
    repo.git(&["checkout", "-q", "--detach"]);
    exercise(&repo);
    assert!(matches!(get_head(&repo.open()).unwrap(), Head::Detached { .. }));
}

#[test]
fn state_no_remotes() {
    let repo = TestRepo::with_commit();
    assert!(repo.git(&["remote"]).trim().is_empty());
    exercise(&repo);
}

#[test]
fn state_bare_repo_errors_clearly() {
    let src = TestRepo::with_commit();
    let bare = tempfile::tempdir().unwrap();
    git_ok(bare.path(), &["clone", "-q", "--bare", src.path().to_str().unwrap(), "."]);
    let mut r = Repository::open(bare.path()).unwrap();
    // Read-only history works.
    assert_eq!(get_commits_live(&r).unwrap().len(), 1);
    assert!(!ref_tips(&r).unwrap().is_empty());
    // Anything needing a working tree is a clear InvalidInput, not a raw libgit2 error.
    let d = [delta("a.txt", DeltaChange::Modified)];
    let is_bare_err = |e: GitYasError| matches!(e, GitYasError::InvalidInput(ref m) if m.contains("bare"));
    assert!(is_bare_err(get_status(&r).unwrap_err()));
    assert!(is_bare_err(stage_files(&r, &d).unwrap_err()));
    assert!(is_bare_err(unstage_files(&r, &d).unwrap_err()));
    assert!(is_bare_err(discard_files(&r, &d).unwrap_err()));
    assert!(is_bare_err(stash_files(&mut r, &sig(), &d, None, false).unwrap_err()));
}

#[test]
fn state_linked_worktree() {
    let main = TestRepo::with_commit();
    let wt_parent = tempfile::tempdir().unwrap();
    let wt_path = wt_parent.path().join("wt");
    main.git(&["worktree", "add", "-q", "-b", "wt-branch", wt_path.to_str().unwrap()]);
    let r = Repository::open(&wt_path).unwrap();
    assert!(r.is_worktree());
    std::fs::write(wt_path.join("in_wt.txt"), "x").unwrap();
    let d = delta("in_wt.txt", DeltaChange::Untracked);
    assert_eq!(get_status(&r).unwrap().unstaged, vec![d.clone()]);
    stage_files(&r, std::slice::from_ref(&d)).unwrap();
    let mut r = Repository::open(&wt_path).unwrap();
    let id = commit_staged_files(&mut r, &sig(), "in worktree", None).unwrap();
    assert!(matches!(get_head(&r).unwrap(), Head::Branch { ref name, .. } if name == "wt-branch"));
    // The main checkout sees the new branch tip in its history; its own status is untouched.
    let m = main.open();
    assert!(get_commits_live(&m).unwrap().iter().any(|c| c.id == id));
    assert!(get_status(&m).unwrap().unstaged.is_empty());
    assert!(!should_bypass_cache(&r).unwrap());
}

#[test]
fn state_submodule() {
    let sub = TestRepo::with_commit();
    let repo = TestRepo::with_commit();
    repo.git(&["submodule", "add", "-q", sub.path().to_str().unwrap(), "sub"]);
    repo.git(&["commit", "-qm", "add submodule"]);
    let r = repo.open();
    assert!(get_status(&r).unwrap().unstaged.is_empty());
    // Move the submodule's HEAD: shows as a modified gitlink, stageable.
    std::fs::write(repo.path().join("sub/extra.txt"), "x").unwrap();
    git_ok(&repo.path().join("sub"), &["add", "-A"]);
    git_ok(&repo.path().join("sub"), &["commit", "-qm", "sub change"]);
    let state = get_status(&r).unwrap();
    assert_eq!(state.unstaged, vec![delta("sub", DeltaChange::Modified)]);
    stage_files(&r, &state.unstaged).unwrap();
    assert_eq!(repo.porcelain(), vec!["M  sub"]);
    let mut r = repo.open();
    let id = commit_staged_files(&mut r, &sig(), "bump sub", None).unwrap();
    assert_eq!(get_commit_files_list(&r, &id).unwrap(), vec![delta("sub", DeltaChange::Modified)]);
    exercise(&repo);
}

#[test]
fn state_shallow_clone() {
    let src = TestRepo::with_commit();
    for i in 0..5 {
        src.write("f", format!("{i}"));
        src.commit_all(&format!("c{i}"));
    }
    let dir = tempfile::tempdir().unwrap();
    git_ok(dir.path(), &["clone", "-q", "--depth", "2", &file_url(src.path()), "."]);
    let shallow = TestRepo { dir };
    let r = shallow.open();
    assert!(r.is_shallow());
    assert!(should_bypass_cache(&r).unwrap());
    let live = get_commits_live(&r).unwrap();
    assert_eq!(live.len(), 2);
    let boundary = live.last().unwrap();
    // The boundary commit is a grafted root: its file list is computed against the empty tree, not an error.
    get_commit_files_list(&r, &boundary.id).unwrap();
    exercise(&shallow);
}

#[test]
fn state_merge_in_progress() {
    let repo = TestRepo::init();
    repo.write("c.txt", "base\n");
    repo.write("other.txt", "o\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-qb", "side"]);
    repo.write("c.txt", "side\n");
    repo.commit_all("side");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("c.txt", "main\n");
    repo.commit_all("main");
    assert!(!repo.git_try(&["merge", "side"]).status.success());
    let r = repo.open();
    let state = get_status(&r).unwrap();
    assert_eq!(state.unstaged, vec![delta("c.txt", DeltaChange::Conflicted)]);
    assert_eq!(get_commits_live(&r).unwrap().len(), 3);
    // Unrelated files can still be staged and hunk-staged mid-merge.
    repo.write("other.txt", "o2\n");
    let d = delta("other.txt", DeltaChange::Modified);
    stage_hunk(&r, &d, &hunk_refs(&r, &d, false)[0]).unwrap();
    assert!(repo.porcelain().contains(&"M  other.txt".to_owned()));
}

#[cfg(unix)]
#[test]
fn state_non_utf8_paths_and_content() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let repo = TestRepo::with_commit();
    let name = OsStr::from_bytes(b"caf\xe9.txt");
    // Some filesystems (e.g. APFS) refuse names that are not valid UTF-8; then only the content part runs elsewhere.
    if std::fs::write(repo.path().join(name), b"\xff\xfe binary-ish \x00 content\n").is_err() {
        eprintln!("filesystem refuses non-UTF-8 file names; skipping path part");
        return;
    }
    let r = repo.open();
    let state = get_status(&r).unwrap();
    assert_eq!(state.unstaged.len(), 1);
    let d = &state.unstaged[0];
    assert_eq!(d.path.as_os_str().as_bytes(), b"caf\xe9.txt", "path bytes preserved exactly");
    stage_files(&r, std::slice::from_ref(d)).unwrap();
    let state = get_status(&r).unwrap();
    assert_eq!(state.staged[0].path.as_os_str().as_bytes(), b"caf\xe9.txt");
    let mut r = repo.open();
    let id = commit_staged_files(&mut r, &sig(), "non-utf8 path", None).unwrap();
    let files = get_commit_files_list(&r, &id).unwrap();
    assert_eq!(files[0].path.as_os_str().as_bytes(), b"caf\xe9.txt");
    discard_files(&r, &files).unwrap();
}

#[test]
fn state_non_utf8_content_hunks() {
    let repo = TestRepo::init();
    repo.write("latin1.txt", b"caf\xe9\nline2\nline3\n");
    repo.commit_all("base");
    repo.write("latin1.txt", b"caf\xe9 au lait\nline2\nline3\n");
    let r = repo.open();
    let d = delta("latin1.txt", DeltaChange::Modified);
    stage_hunk(&r, &d, &hunk_refs(&r, &d, false)[0]).unwrap();
    assert_eq!(repo.porcelain(), vec!["M  latin1.txt"]);
}

#[test]
fn state_large_file_and_long_history() {
    let repo = TestRepo::init();
    // ~8 MB file with many lines: status, staging and a hunk op still work.
    let big: String = (0..400_000).map(|i| format!("line number {i:08}\n")).collect();
    repo.write("big.txt", &big);
    repo.commit_all("big");
    let edited = big.replacen("line number 00200000\n", "EDITED\n", 1);
    repo.write("big.txt", &edited);
    let r = repo.open();
    let d = delta("big.txt", DeltaChange::Modified);
    assert_eq!(get_status(&r).unwrap().unstaged, vec![d.clone()]);
    stage_hunk(&r, &d, &hunk_refs(&r, &d, false)[0]).unwrap();
    assert_eq!(repo.porcelain(), vec!["M  big.txt"]);

    // Long history via fast-import (2000 commits).
    let mut script = String::new();
    for i in 0..2000 {
        script.push_str(&format!(
            "commit refs/heads/long\nmark :{m}\ncommitter T <t@e> {t} +0000\ndata 3\nc{n:02}\n",
            m = i + 1,
            t = 1_600_000_000 + i,
            n = i % 100
        ));
        if i > 0 {
            script.push_str(&format!("from :{}\n", i));
        }
        script.push_str(&format!("M 644 inline f\ndata {}\n{}\n", format!("{i}").len() + 1, i));
    }
    let script_path = repo.path().join(".git/import.txt");
    std::fs::write(&script_path, script).unwrap();
    let out = std::process::Command::new("git")
        .args(["fast-import", "--quiet"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::fs::File::open(&script_path).unwrap())
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let r = repo.open();
    let live = get_commits_live(&r).unwrap();
    assert_eq!(live.len(), 2001);
    let tips = ref_tips(&r).unwrap();
    assert_eq!(walk_new_commits(&r, &tips, &[]).unwrap().len(), 2001);
}

/// Non-UTF-8 paths without touching the filesystem (APFS rejects such names): the entry exists only in git.
#[cfg(unix)]
#[test]
fn state_non_utf8_path_in_index_and_history() {
    use std::os::unix::ffi::OsStrExt;
    let repo = TestRepo::with_commit();
    let mut r = repo.open();
    let raw = b"caf\xe9.txt".to_vec();
    {
        let blob = r.blob(b"content\n").unwrap();
        let mut index = r.index().unwrap();
        index
            .add(&git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: 0o100644,
                uid: 0,
                gid: 0,
                file_size: 0,
                id: blob,
                flags: 0,
                flags_extended: 0,
                path: raw.clone(),
            })
            .unwrap();
        index.write().unwrap();
    }
    let id = commit_staged_files(&mut r, &sig(), "latin-1 file name", None).unwrap();
    let files = get_commit_files_list(&r, &id).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path.as_os_str().as_bytes(), raw.as_slice(), "path bytes preserved exactly");
    // Not on disk: shows as deleted in the working tree, with the exact bytes, and the deletion can be staged.
    let state = get_status(&r).unwrap();
    assert_eq!(state.unstaged.len(), 1);
    assert_eq!(state.unstaged[0].change, DeltaChange::Deleted);
    assert_eq!(state.unstaged[0].path.as_os_str().as_bytes(), raw.as_slice());
    stage_files(&r, &state.unstaged).unwrap();
    let state = get_status(&r).unwrap();
    assert_eq!(state.staged.len(), 1);
    assert_eq!(state.staged[0].path.as_os_str().as_bytes(), raw.as_slice());
    unstage_files(&r, &state.staged).unwrap();
    assert_eq!(get_status(&r).unwrap().staged.len(), 0);
}
