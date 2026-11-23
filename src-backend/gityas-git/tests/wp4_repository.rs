//! WP4: repository open / init / clone / state queries / remotes.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test crate: panics are the assertions

mod wp4_support;

use std::sync::atomic::AtomicBool;

use gityas_core::{GitYasError, Provider};
use gityas_git::remote::{NoCredentials, RemoteContext};
use gityas_git::repository::{self, RepoState};
use tempfile::TempDir;
use wp4_support::*;

#[test]
fn open_rejects_non_repo_and_discover_searches_up() {
    let tmp = TempDir::new().unwrap();
    assert!(matches!(repository::open(tmp.path()), Err(GitYasError::NotFound(_))));
    let repo = init(&tmp.path().join("r"));
    std::fs::create_dir_all(tmp.path().join("r/sub/dir")).unwrap();
    assert!(repository::open(&tmp.path().join("r/sub/dir")).is_err());
    let found = repository::discover(&tmp.path().join("r/sub/dir")).unwrap();
    assert_eq!(found.path(), repo.path());
}

#[test]
fn unborn_repo_info() {
    let tmp = TempDir::new().unwrap();
    let repo = init(tmp.path());
    let info = repository::info(&repo).unwrap();
    assert!(info.head_unborn);
    assert!(!info.head_detached && !info.is_bare && !info.is_shallow && !info.is_worktree);
    assert_eq!(info.state, RepoState::Clean);
    assert_eq!(repo.head().err().unwrap().code(), git2::ErrorCode::UnbornBranch);
    assert_eq!(
        repository::repository_name(&repo).unwrap(),
        tmp.path().file_name().unwrap().to_str().unwrap()
    );
    assert!(repository::remote_names(&repo).unwrap().is_empty());
}

#[test]
fn init_rejects_invalid_branch() {
    let tmp = TempDir::new().unwrap();
    assert!(matches!(
        repository::init(tmp.path(), false, Some("bad..name")),
        Err(GitYasError::InvalidInput(_))
    ));
}

#[test]
fn bare_repo_info_and_name() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("proj.git");
    repository::init(&path, true, None).unwrap();
    let repo = repository::open(&path).unwrap();
    let info = repository::info(&repo).unwrap();
    assert!(info.is_bare && info.workdir.is_none() && info.head_unborn);
    assert_eq!(repository::repository_name(&repo).unwrap(), "proj");
}

#[test]
fn linked_worktree_opens_and_reports() {
    let tmp = TempDir::new().unwrap();
    let repo = init(&tmp.path().join("main"));
    commit_file(&repo, "a", "1\n", "c1");
    let wt_path = tmp.path().join("wt");
    repo.worktree("wt", &wt_path, None).unwrap();
    let wt = repository::open(&wt_path).unwrap();
    let info = repository::info(&wt).unwrap();
    assert!(info.is_worktree && !info.is_bare);
    assert_eq!(
        info.workdir.as_deref().map(|p| p.canonicalize().unwrap()),
        Some(wt_path.canonicalize().unwrap())
    );
    assert_eq!(repository::repository_name(&wt).unwrap(), "wt");
}

#[test]
fn detached_and_merge_state() {
    let fx = Fixture::new();
    let c1 = head_oid(&fx.work);
    fx.work.set_head_detached(c1).unwrap();
    assert!(repository::info(&fx.work).unwrap().head_detached);
    // Fake a merge in progress.
    std::fs::write(fx.work.path().join("MERGE_HEAD"), format!("{c1}\n")).unwrap();
    assert!(repository::is_merge_in_progress(&fx.work));
    assert!(!repository::is_rebase_in_progress(&fx.work));
    assert_eq!(repository::info(&fx.work).unwrap().state, RepoState::Merge);
}

#[test]
fn list_remotes_detects_provider_and_ssh() {
    let tmp = TempDir::new().unwrap();
    let repo = init(tmp.path());
    repo.remote("origin", "https://github.com/o/r.git").unwrap();
    repo.remote("lab", "git@gitlab.com:g/r.git").unwrap();
    repo.remote("team/fork", "https://example.com/r.git").unwrap();
    repo.remote_set_pushurl("origin", Some("https://dev.azure.com/o/p/_git/r")).unwrap();
    let remotes = repository::list_remotes(&repo).unwrap();
    let by_name = |n: &str| remotes.iter().find(|r| r.name == n).unwrap();
    assert_eq!(by_name("origin").provider, Some(Provider::AzureDevops)); // push URL wins
    assert!(!by_name("origin").is_ssh);
    assert_eq!(by_name("lab").provider, Some(Provider::Gitlab));
    assert!(by_name("lab").is_ssh);
    assert_eq!(by_name("team/fork").provider, None);
    let mut names = repository::remote_names(&repo).unwrap();
    names.sort();
    assert_eq!(names, ["lab", "origin", "team/fork"]);
}

#[test]
fn clone_local_remote_with_progress() {
    let fx = Fixture::new();
    let dest = repository::clone_destination(&file_url(&fx.remote_path), &fx.root.join("clones")).unwrap();
    assert_eq!(dest, fx.root.join("clones/remote"));
    let events = std::cell::RefCell::new(0usize);
    let report = |_: &gityas_git::remote::TransferProgress| *events.borrow_mut() += 1;
    let creds = NoCredentials;
    let ctx = RemoteContext::new(&creds).with_progress(&report).with_credential_helper(false);
    let cloned = repository::clone(&file_url(&fx.remote_path), &dest, &ctx).unwrap();
    assert_eq!(head_oid(&cloned), head_oid(&fx.work));
    assert!(*events.borrow() > 0, "progress callback was never invoked");
    // Oracle: the CLI sees a clean clone tracking origin/main. `status` runs with the machine's git config, as the
    // clone did: libgit2 checks out with the system `core.autocrlf` (Git for Windows sets `true`), and an isolated
    // CLI would report every converted file as modified.
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&dest)
        .output()
        .unwrap();
    assert!(status.status.success());
    assert_eq!(String::from_utf8_lossy(&status.stdout), "");
    assert_eq!(git(&dest, &["rev-parse", "--abbrev-ref", "main@{upstream}"]).trim(), "origin/main");
}

#[test]
fn clone_checks_out_paths_longer_than_max_path() {
    // Built in a bare repo with libgit2 so the source never needs a long working-tree path itself.
    let tmp = TempDir::new().unwrap();
    let bare = git2::Repository::init_bare(tmp.path().join("long.git")).unwrap();
    let segment = "s".repeat(60);
    let mut tree = {
        let blob = bare.blob(b"deep\n").unwrap();
        let mut b = bare.treebuilder(None).unwrap();
        b.insert("file.txt", blob, 0o100644).unwrap();
        b.write().unwrap()
    };
    for _ in 0..5 {
        let mut b = bare.treebuilder(None).unwrap();
        b.insert(&segment, tree, 0o040000).unwrap();
        tree = b.write().unwrap();
    }
    let sig = git2::Signature::now("Test User", "test@example.com").unwrap();
    let tree = bare.find_tree(tree).unwrap();
    bare.commit(Some("refs/heads/main"), &sig, &sig, "long", &tree, &[]).unwrap();
    bare.set_head("refs/heads/main").unwrap();

    let dest = tmp.path().join("clone");
    let creds = NoCredentials;
    let ctx = RemoteContext::new(&creds).with_credential_helper(false);
    let cloned = repository::clone(&file_url(&tmp.path().join("long.git")), &dest, &ctx).unwrap();
    let rel: std::path::PathBuf = std::iter::repeat_n(segment.as_str(), 5).chain(["file.txt"]).collect();
    assert!(dest.join(&rel).as_os_str().len() > 260);
    assert!(cloned.workdir().unwrap().join(&rel).is_file());
}

#[test]
fn clone_refuses_non_empty_destination_and_cleans_up_failed_ssh() {
    let fx = Fixture::new();
    let dest = fx.root.join("busy");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("x"), "x").unwrap();
    let creds = NoCredentials;
    let ctx = RemoteContext::new(&creds).with_credential_helper(false);
    assert!(matches!(
        repository::clone(&file_url(&fx.remote_path), &dest, &ctx),
        Err(GitYasError::InvalidInput(_))
    ));
    // SSH is attempted (a closed local port here, so no network) and a failure leaves nothing behind.
    let err = repository::clone("ssh://git@127.0.0.1:1/o/r.git", &fx.root.join("ssh"), &ctx)
        .err()
        .unwrap();
    assert!(!err.to_string().contains("not supported"), "{err:?}");
    assert!(!fx.root.join("ssh").exists());
}

#[test]
fn clone_cancelled() {
    let fx = Fixture::new();
    let cancel = AtomicBool::new(true);
    let creds = NoCredentials;
    let ctx = RemoteContext::new(&creds).with_cancel(&cancel);
    let err = repository::clone(&file_url(&fx.remote_path), &fx.root.join("c"), &ctx)
        .err()
        .unwrap();
    assert_eq!(err, GitYasError::Cancelled);

    // Cancel mid-transfer: the progress callback flips the flag.
    let cancel = AtomicBool::new(false);
    let flip = |_: &gityas_git::remote::TransferProgress| cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    let ctx = RemoteContext::new(&creds).with_cancel(&cancel).with_progress(&flip);
    let err = repository::clone(&file_url(&fx.remote_path), &fx.root.join("c2"), &ctx)
        .err()
        .unwrap();
    assert_eq!(err, GitYasError::Cancelled);
}

#[test]
fn shallow_clone_is_reported() {
    let fx = Fixture::new();
    commit_file(&fx.work, "b", "2\n", "c2");
    raw_push(&fx.work, "refs/heads/main:refs/heads/main");
    let dest = fx.root.join("shallow");
    // libgit2's local transport can't do shallow fetches; the CLI (test-only) creates the fixture.
    git(
        &fx.root,
        &["clone", "--depth", "1", &file_url(&fx.remote_path), dest.to_str().unwrap()],
    );
    let repo = repository::open(&dest).unwrap();
    assert!(repository::info(&repo).unwrap().is_shallow);
    // Branch listing (ahead/behind over truncated history) still works.
    let branches = gityas_git::branches::list_branches(&repo, None).unwrap();
    assert!(
        branches
            .iter()
            .any(|b| b.refname == "refs/heads/main" && b.ahead == 0 && b.behind == 0)
    );
}
