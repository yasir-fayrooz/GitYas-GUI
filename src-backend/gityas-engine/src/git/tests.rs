//! Facade tests: every command group through the engine, asserting both the returned value and the emitted
//! events. Temp repositories built with git2; a local bare repository is the remote.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use git2::Repository;
use gityas_auth::MemoryCredentialStore;
use gityas_core::diff::{DiffTarget, HunkRef};
use gityas_core::events::RecordingSink;
use gityas_core::models::{
    BranchKind, CommitSignature, DeletedMergeAction, Delta, DeltaChange, Head, MergeOutcome, RemoteOp, RemoteProgress, StashApplyOutcome,
};
use gityas_core::{AppEvent, EventSink, GitYasError, Oid};
use gityas_git::remote::TransferProgress;
use gityas_storage::Storage;
use tempfile::TempDir;

use crate::progress::{MIN_INTERVAL, ProgressReporter, describe};
use crate::testutil::{TestEngine, test_engine};
use crate::{CheckoutTarget, Engine, EngineOptions};

// ---- helpers -----------------------------------------------------------------------------------------------

const HEAD_EVENTS: [&str; 5] = [
    "CommitsUpdated",
    "CommitsHeadUpdated",
    "LocalBranchesUpdated",
    "GitFilesChanged",
    "RepoStatusUpdated",
];
/// `Refresh::FILES`: status plus the repository state (a merge can end by resolving or aborting).
const FILES_EVENTS: [&str; 2] = ["GitFilesChanged", "RepoStatusUpdated"];
const REMOTE_REFS_EVENTS: [&str; 4] = ["CommitsUpdated", "LocalBranchesUpdated", "RemoteBranchesUpdated", "RemotesUpdated"];
/// Fetch and pull refresh every ref list.
const ALL_REFS_EVENTS: [&str; 6] = [
    "CommitsUpdated",
    "CommitsHeadUpdated",
    "LocalBranchesUpdated",
    "RemoteBranchesUpdated",
    "RemotesUpdated",
    "StashesUpdated",
];

fn init(dir: &Path) -> Repository {
    let repo = gityas_git::repository::init(dir, false, Some("main")).unwrap();
    // Git for Windows enables `core.autocrlf` in its system config, which libgit2 honours: pin it so checkouts are byte-exact.
    repo.config().unwrap().set_bool("core.autocrlf", false).unwrap();
    repo
}

fn write(repo: &Repository, rel: &str, content: &str) {
    let path = repo.workdir().unwrap().join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn read(repo: &Repository, rel: &str) -> String {
    std::fs::read_to_string(repo.workdir().unwrap().join(rel)).unwrap()
}

/// Writes `files`, stages everything and commits on HEAD (git2 directly, not through the engine).
fn commit(repo: &Repository, files: &[(&str, &str)], message: &str) -> git2::Oid {
    for (rel, content) in files {
        write(repo, rel, content);
    }
    let mut index = repo.index().unwrap();
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    let parents: Vec<git2::Commit<'_>> = repo.head().ok().and_then(|h| h.peel_to_commit().ok()).into_iter().collect();
    let parents: Vec<&git2::Commit<'_>> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents).unwrap()
}

fn oid(id: git2::Oid) -> Oid {
    gityas_git::convert::oid_from_git(id).unwrap()
}

fn modified(path: &str) -> Delta {
    Delta {
        path: PathBuf::from(path),
        old_path: None,
        change: DeltaChange::Modified,
    }
}

fn untracked(path: &str) -> Delta {
    Delta {
        path: PathBuf::from(path),
        old_path: None,
        change: DeltaChange::Untracked,
    }
}

fn set_signature(t: &TestEngine) {
    t.engine
        .inner
        .storage
        .user()
        .set_commit_signature(&CommitSignature::new("Engine Test", "engine@example.com"))
        .unwrap();
}

/// Names of the non-progress events taken from the sink.
fn names(t: &TestEngine) -> Vec<&'static str> {
    t.sink
        .take()
        .iter()
        .map(AppEvent::name)
        .filter(|n| *n != "RemoteProgress")
        .collect()
}

fn progress_events(events: &[AppEvent]) -> Vec<RemoteProgress> {
    events
        .iter()
        .filter_map(|e| match e {
            AppEvent::RemoteProgress { progress, .. } => Some(progress.clone()),
            _ => None,
        })
        .collect()
}

/// A working repo with one commit and an `origin` bare remote.
struct WithRemote {
    _dirs: (TempDir, TempDir),
    work: Repository,
    bare_path: PathBuf,
}

fn with_remote() -> WithRemote {
    let work_dir = tempfile::tempdir().unwrap();
    let bare_dir = tempfile::tempdir().unwrap();
    gityas_git::repository::init(bare_dir.path(), true, Some("main")).unwrap();
    let work = init(work_dir.path());
    commit(&work, &[("f.txt", "one\ntwo\nthree\n")], "initial");
    work.remote("origin", bare_dir.path().to_str().unwrap()).unwrap();
    let bare_path = bare_dir.path().to_path_buf();
    WithRemote {
        _dirs: (work_dir, bare_dir),
        work,
        bare_path,
    }
}

/// A second clone of the bare remote that pushes with git2 directly (simulates another user).
fn other_clone(bare: &Path) -> (TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::clone(bare.to_str().unwrap(), dir.path()).unwrap();
    repo.config().unwrap().set_bool("core.autocrlf", false).unwrap();
    (dir, repo)
}

fn git2_push(repo: &Repository, refspec: &str) {
    repo.find_remote("origin").unwrap().push(&[refspec], None).unwrap();
}

// ---- no session ----------------------------------------------------------------------------------------------

type Call = Pin<Box<dyn Future<Output = gityas_core::Result<()>> + Send>>;

fn every_command(e: &Engine) -> Vec<(&'static str, Call)> {
    let o = Oid::from_bytes(&[1; 20]).unwrap();
    fn d() -> Delta {
        modified("f.txt")
    }
    fn h() -> HunkRef {
        HunkRef {
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
            content_hash: 0,
        }
    }
    let e = e.clone();
    macro_rules! call {
        ($name:literal, $fut:expr) => {{
            let e = e.clone();
            let o = o.clone();
            let f = $fut;
            ($name, Box::pin(async move { f(e, o).await.map(drop) }) as Call)
        }};
    }
    vec![
        call!("head", |e: Engine, _o: Oid| async move { e.head().await }),
        call!("commit_body", |e: Engine, o: Oid| async move { e.commit_body(o).await }),
        call!("commit_staged_files", |e: Engine, _o: Oid| async move {
            e.commit_staged_files("m".into(), None).await
        }),
        call!(
            "reword_commit",
            |e: Engine, o: Oid| async move { e.reword_commit(o, "m".into()).await }
        ),
        call!(
            "create_tag",
            |e: Engine, o: Oid| async move { e.create_tag(o, "t".into(), None).await }
        ),
        call!("delete_tag", |e: Engine, _o: Oid| async move { e.delete_tag("t".into()).await }),
        call!("push_tag", |e: Engine, _o: Oid| async move {
            e.push_tag("origin".into(), "t".into()).await
        }),
        call!("delete_remote_tag", |e: Engine, _o: Oid| async move {
            e.delete_remote_tag("origin".into(), "t".into()).await
        }),
        call!("remote_tag_names", |e: Engine, _o: Oid| async move {
            e.remote_tag_names("origin".into()).await
        }),
        call!("branches", |e: Engine, _o: Oid| async move { e.branches(BranchKind::Local).await }),
        call!("remotes", |e: Engine, _o: Oid| async move { e.remotes().await }),
        call!("checkout", |e: Engine, o: Oid| async move {
            e.checkout(CheckoutTarget::Commit(o)).await
        }),
        call!("create_branch", |e: Engine, _o: Oid| async move {
            e.create_branch("b".into(), None, false).await
        }),
        call!("create_remote_branch", |e: Engine, _o: Oid| async move {
            e.create_remote_branch("origin".into(), "b".into()).await
        }),
        call!("publish_branch", |e: Engine, _o: Oid| async move {
            e.publish_branch("refs/heads/main".into(), "origin".into(), "b".into())
                .await
                .map(drop)
        }),
        call!("delete_branch", |e: Engine, _o: Oid| async move {
            e.delete_branch("refs/heads/b".into(), false).await
        }),
        call!("fetch", |e: Engine, _o: Oid| async move { e.fetch(None).await }),
        call!("pull", |e: Engine, _o: Oid| async move { e.pull("refs/heads/main".into()).await }),
        call!(
            "push",
            |e: Engine, _o: Oid| async move { e.push("refs/heads/main".into(), false).await }
        ),
        call!("merge", |e: Engine, _o: Oid| async move {
            e.merge("refs/heads/a".into(), "refs/heads/b".into()).await
        }),
        call!("abort_merge", |e: Engine, _o: Oid| async move { e.abort_merge().await }),
        call!("file_state", |e: Engine, _o: Oid| async move { e.file_state().await }),
        call!("stage_files", |e: Engine, _o: Oid| async move { e.stage_files(vec![d()]).await }),
        call!(
            "unstage_files",
            |e: Engine, _o: Oid| async move { e.unstage_files(vec![d()]).await }
        ),
        call!(
            "discard_files",
            |e: Engine, _o: Oid| async move { e.discard_files(vec![d()]).await }
        ),
        call!("stage_hunk", |e: Engine, _o: Oid| async move { e.stage_hunk(d(), h()).await }),
        call!("unstage_hunk", |e: Engine, _o: Oid| async move { e.unstage_hunk(d(), h()).await }),
        call!("discard_hunk", |e: Engine, _o: Oid| async move { e.discard_hunk(d(), h()).await }),
        call!("stash_files", |e: Engine, _o: Oid| async move {
            e.stash_files(vec![d()], None, true).await
        }),
        call!("stash_all", |e: Engine, _o: Oid| async move { e.stash_all(None, true).await }),
        call!("stashes", |e: Engine, _o: Oid| async move { e.stashes().await }),
        call!("apply_stash", |e: Engine, _o: Oid| async move { e.apply_stash(0).await }),
        call!("pop_stash", |e: Engine, _o: Oid| async move { e.pop_stash(0).await }),
        call!("delete_stash", |e: Engine, _o: Oid| async move { e.delete_stash(0).await }),
        call!("file_diff", |e: Engine, _o: Oid| async move {
            e.file_diff(DiffTarget::WorkingTree { staged: false }, d()).await
        }),
        call!("file_diff_with_limit", |e: Engine, _o: Oid| async move {
            e.file_diff_with_limit(DiffTarget::WorkingTree { staged: false }, d(), 1024).await
        }),
        call!("merge_conflict", |e: Engine, _o: Oid| async move {
            e.merge_conflict("f.txt".into()).await
        }),
        call!("resolve_conflict", |e: Engine, _o: Oid| async move {
            e.resolve_conflict("f.txt".into(), "x".into()).await
        }),
        call!("resolve_conflict_with_action", |e: Engine, _o: Oid| async move {
            e.resolve_conflict_with_action("f.txt".into(), DeletedMergeAction::TakeOurs).await
        }),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn every_command_without_a_session_is_no_active_repository() {
    let t = test_engine();
    let calls = every_command(&t.engine);
    assert_eq!(calls.len(), 39);
    for (name, call) in calls {
        assert_eq!(call.await.err(), Some(GitYasError::NoActiveRepository), "{name}");
    }
    assert!(t.sink.take().is_empty());
}

// ---- read-only queries ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn read_only_queries_return_data_and_emit_nothing() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    let c = commit(&repo, &[("f.txt", "a\n")], "subject\n\nbody text\n");
    write(&repo, "f.txt", "a\nb\n");
    t.install_session(dir.path());

    match t.engine.head().await.unwrap() {
        Some(Head::Branch { refname, target, .. }) => {
            assert_eq!(refname, "refs/heads/main");
            assert_eq!(target, oid(c));
        }
        other => panic!("unexpected head {other:?}"),
    }
    let body = t.engine.commit_body(oid(c)).await.unwrap();
    assert!(body.message.contains("body text"));
    assert!(!body.lossy);
    let local = t.engine.branches(BranchKind::Local).await.unwrap();
    assert_eq!(local.iter().map(|b| b.refname.as_str()).collect::<Vec<_>>(), ["refs/heads/main"]);
    assert!(t.engine.branches(BranchKind::Remote).await.unwrap().is_empty());
    assert!(t.engine.remotes().await.unwrap().is_empty());
    let state = t.engine.file_state().await.unwrap();
    assert_eq!(state.unstaged, vec![modified("f.txt")]);
    assert!(state.staged.is_empty());
    assert!(t.engine.stashes().await.unwrap().is_empty());
    let diff = t
        .engine
        .file_diff(DiffTarget::WorkingTree { staged: false }, modified("f.txt"))
        .await
        .unwrap();
    assert_eq!(diff.hunks.len(), 1);
    assert!(matches!(
        t.engine.merge_conflict("f.txt".into()).await,
        Err(GitYasError::NotFound(_))
    ));
    assert!(t.sink.take().is_empty(), "read-only queries must not emit");
}

#[tokio::test(flavor = "multi_thread")]
async fn file_diff_uses_the_engine_size_cap() {
    let mut options = EngineOptions {
        fetch_interval: None,
        ..EngineOptions::default()
    };
    options.diff.max_bytes = 4;
    let t = crate::testutil::test_engine_with(options);
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    commit(&repo, &[("f.txt", "a\n")], "c");
    write(&repo, "f.txt", "a much longer line\n");
    t.install_session(dir.path());
    let diff = t
        .engine
        .file_diff(DiffTarget::WorkingTree { staged: false }, modified("f.txt"))
        .await
        .unwrap();
    assert!(matches!(diff.kind, gityas_core::diff::DiffKind::TooLarge { .. }), "{:?}", diff.kind);

    // "Show anyway": a per-call cap overrides the engine's; still capped when it is too small.
    let shown = t
        .engine
        .file_diff_with_limit(DiffTarget::WorkingTree { staged: false }, modified("f.txt"), 1024)
        .await
        .unwrap();
    assert_eq!(shown.kind, gityas_core::diff::DiffKind::Text);
    assert_eq!(shown.hunks.len(), 1);
    let still = t
        .engine
        .file_diff_with_limit(DiffTarget::WorkingTree { staged: false }, modified("f.txt"), 2)
        .await
        .unwrap();
    assert!(matches!(still.kind, gityas_core::diff::DiffKind::TooLarge { .. }));
    assert!(t.sink.take().is_empty(), "read-only query must not emit");
}

// ---- commits ---------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn stage_then_commit_on_unborn_head_emits_files_then_head_events() {
    let t = test_engine();
    set_signature(&t);
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    write(&repo, "new.txt", "hello\n");
    t.install_session(dir.path());
    assert!(t.engine.head().await.unwrap().is_some_and(|h| h.is_unborn()));

    t.engine.stage_files(vec![untracked("new.txt")]).await.unwrap();
    let events = t.sink.take();
    assert_eq!(events.iter().map(AppEvent::name).collect::<Vec<_>>(), FILES_EVENTS);
    match &events[0] {
        AppEvent::GitFilesChanged { files, .. } => {
            assert_eq!(files.staged.len(), 1);
            assert_eq!(files.staged[0].path, Path::new("new.txt"));
            assert!(files.unstaged.is_empty());
        }
        other => panic!("unexpected {other:?}"),
    }

    let id = t.engine.commit_staged_files("first".into(), Some("".into())).await.unwrap();
    assert_eq!(names(&t), HEAD_EVENTS);
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(oid(head.id()), id);
    assert_eq!(head.message(), Some("first\n"));
    assert_eq!(head.author().name(), Some("Engine Test"));
    assert_eq!(head.parent_count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_without_signature_is_a_clear_error_and_emits_nothing() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    write(&repo, "f.txt", "x\n");
    t.install_session(dir.path());
    t.engine.stage_files(vec![untracked("f.txt")]).await.unwrap();
    t.sink.take();
    match t.engine.commit_staged_files("m".into(), None).await {
        Err(GitYasError::SignatureRequired(msg)) => assert!(msg.contains("signature"), "{msg}"),
        other => panic!("unexpected {other:?}"),
    }
    assert!(t.sink.take().is_empty());
    assert!(repo.head().is_err(), "nothing committed");
}

// ---- hunks ----------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn stale_hunk_is_rejected_and_a_fresh_one_stages() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    commit(&repo, &[("f.txt", "one\ntwo\nthree\n")], "c");
    write(&repo, "f.txt", "one\nTWO\nthree\n");
    t.install_session(dir.path());
    let unstaged = DiffTarget::WorkingTree { staged: false };
    let shown = t.engine.file_diff(unstaged.clone(), modified("f.txt")).await.unwrap();
    let stale = shown.hunks[0].id;

    write(&repo, "f.txt", "one\nZWEI\nthree\n");
    assert_eq!(t.engine.stage_hunk(modified("f.txt"), stale).await, Err(GitYasError::StaleHunk));
    assert_eq!(t.engine.discard_hunk(modified("f.txt"), stale).await, Err(GitYasError::StaleHunk));
    assert!(t.sink.take().is_empty());
    assert_eq!(read(&repo, "f.txt"), "one\nZWEI\nthree\n", "nothing changed");

    let fresh = t.engine.file_diff(unstaged, modified("f.txt")).await.unwrap().hunks[0].id;
    t.engine.stage_hunk(modified("f.txt"), fresh).await.unwrap();
    assert_eq!(names(&t), FILES_EVENTS);
    let state = t.engine.file_state().await.unwrap();
    assert_eq!(state.staged, vec![modified("f.txt")]);
    assert!(state.unstaged.is_empty());

    let staged_hunk = t
        .engine
        .file_diff(DiffTarget::WorkingTree { staged: true }, modified("f.txt"))
        .await
        .unwrap()
        .hunks[0]
        .id;
    t.engine.unstage_hunk(modified("f.txt"), staged_hunk).await.unwrap();
    assert_eq!(names(&t), FILES_EVENTS);
    let hunk = t
        .engine
        .file_diff(DiffTarget::WorkingTree { staged: false }, modified("f.txt"))
        .await
        .unwrap()
        .hunks[0]
        .id;
    t.engine.discard_hunk(modified("f.txt"), hunk).await.unwrap();
    assert_eq!(names(&t), FILES_EVENTS);
    assert_eq!(read(&repo, "f.txt"), "one\ntwo\nthree\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn unstage_and_discard_files_emit_files_only() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    commit(&repo, &[("f.txt", "a\n")], "c");
    write(&repo, "f.txt", "b\n");
    t.install_session(dir.path());
    t.engine.stage_files(vec![modified("f.txt")]).await.unwrap();
    t.engine.unstage_files(vec![modified("f.txt")]).await.unwrap();
    t.engine.discard_files(vec![modified("f.txt")]).await.unwrap();
    assert_eq!(names(&t), [FILES_EVENTS; 3].concat());
    assert_eq!(read(&repo, "f.txt"), "a\n");
    assert_eq!(t.engine.file_state().await.unwrap(), Default::default());
}

// ---- tags -----------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn tags_local_and_remote() {
    let t = test_engine();
    let r = with_remote();
    let head = oid(r.work.head().unwrap().target().unwrap());
    t.install_session(r.work.workdir().unwrap());

    t.engine.create_tag(head.clone(), "v1".into(), None).await.unwrap();
    assert_eq!(names(&t), ["CommitsUpdated"]);
    assert!(r.work.find_reference("refs/tags/v1").is_ok());

    match t.engine.create_tag(head.clone(), "v2".into(), Some("release".into())).await {
        Err(GitYasError::SignatureRequired(msg)) => assert!(msg.contains("signature"), "{msg}"),
        other => panic!("annotated tag without signature: {other:?}"),
    }
    set_signature(&t);
    t.engine.create_tag(head, "v2".into(), Some("release".into())).await.unwrap();
    assert_eq!(names(&t), ["CommitsUpdated"]);
    assert!(r.work.find_reference("refs/tags/v2").unwrap().peel_to_tag().is_ok());

    t.engine.push_tag("origin".into(), "v1".into()).await.unwrap();
    assert!(names(&t).is_empty(), "push_tag changes no local ref");
    let bare = Repository::open_bare(&r.bare_path).unwrap();
    assert!(bare.find_reference("refs/tags/v1").is_ok());
    assert_eq!(t.engine.remote_tag_names("origin".into()).await.unwrap(), ["v1"]);
    assert!(names(&t).is_empty());

    t.engine.delete_remote_tag("origin".into(), "v1".into()).await.unwrap();
    assert!(names(&t).is_empty());
    assert!(bare.find_reference("refs/tags/v1").is_err());
    assert!(t.engine.remote_tag_names("origin".into()).await.unwrap().is_empty());

    t.engine.delete_tag("v1".into()).await.unwrap();
    assert_eq!(names(&t), ["CommitsUpdated"]);
    assert!(r.work.find_reference("refs/tags/v1").is_err());
}

// ---- branches -------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn branch_create_checkout_detach_and_delete() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    let first = commit(&repo, &[("f.txt", "1\n")], "one");
    let second = commit(&repo, &[("f.txt", "2\n")], "two");
    t.install_session(dir.path());

    let refname = t.engine.create_branch("feature".into(), Some(oid(first)), false).await.unwrap();
    assert_eq!(refname, "refs/heads/feature");
    assert_eq!(names(&t), ["CommitsUpdated", "LocalBranchesUpdated"]);
    assert_eq!(repo.head().unwrap().name(), Some("refs/heads/main"));

    t.engine.checkout(CheckoutTarget::Branch(refname.clone())).await.unwrap();
    assert_eq!(names(&t), HEAD_EVENTS);
    assert_eq!(read(&repo, "f.txt"), "1\n");

    // Detached HEAD at a commit.
    t.engine.checkout(CheckoutTarget::Commit(oid(second))).await.unwrap();
    let events = t.sink.take();
    assert!(events.iter().any(|e| matches!(
        e,
        AppEvent::CommitsHeadUpdated { head: Some(Head::Detached { target }), .. } if *target == oid(second)
    )));
    assert!(t.engine.head().await.unwrap().is_some_and(|h| h.is_detached()));
    assert_eq!(read(&repo, "f.txt"), "2\n");
    // Publishing a detached HEAD is refused before any network access.
    assert!(matches!(
        t.engine.create_remote_branch("origin".into(), "x".into()).await,
        Err(GitYasError::InvalidInput(_))
    ));

    let created = t.engine.create_branch("other".into(), None, true).await.unwrap();
    assert_eq!(names(&t), HEAD_EVENTS);
    assert_eq!(repo.head().unwrap().name(), Some(created.as_str()));

    // `feature` (at `first`) is merged into HEAD (`second`), so a safe delete works.
    t.engine.delete_branch(refname, false).await.unwrap();
    assert_eq!(names(&t), ["CommitsUpdated", "LocalBranchesUpdated"]);
    assert!(repo.find_reference("refs/heads/feature").is_err());
    assert!(matches!(
        t.engine.delete_branch("refs/tags/x".into(), false).await,
        Err(GitYasError::InvalidInput(_))
    ));
    assert!(t.sink.take().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn create_and_delete_remote_branch() {
    let t = test_engine();
    let r = with_remote();
    t.install_session(r.work.workdir().unwrap());
    t.engine.create_remote_branch("origin".into(), "published".into()).await.unwrap();
    assert_eq!(names(&t), REMOTE_REFS_EVENTS);
    let bare = Repository::open_bare(&r.bare_path).unwrap();
    assert!(bare.find_reference("refs/heads/published").is_ok());
    let main = t.engine.branches(BranchKind::Local).await.unwrap();
    assert_eq!(main[0].upstream.as_deref(), Some("origin/published"));

    t.engine.delete_branch("refs/remotes/origin/published".into(), false).await.unwrap();
    assert_eq!(names(&t), REMOTE_REFS_EVENTS);
    assert!(bare.find_reference("refs/heads/published").is_err());
    assert!(r.work.find_reference("refs/remotes/origin/published").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn publish_branch_pushes_a_branch_that_is_not_checked_out() {
    let t = test_engine();
    let r = with_remote();
    r.work.remote("upstream", r.bare_path.to_str().unwrap()).unwrap();
    t.install_session(r.work.workdir().unwrap());
    t.engine.create_branch("side".into(), None, false).await.unwrap();
    t.sink.take();
    let upstream = t
        .engine
        .publish_branch("refs/heads/side".into(), "upstream".into(), "renamed".into())
        .await
        .unwrap();
    assert_eq!(upstream, "upstream/renamed");
    assert_eq!(names(&t), REMOTE_REFS_EVENTS);
    let bare = Repository::open_bare(&r.bare_path).unwrap();
    assert!(bare.find_reference("refs/heads/renamed").is_ok());
    let side = t
        .engine
        .branches(BranchKind::Local)
        .await
        .unwrap()
        .into_iter()
        .find(|b| b.name == "side")
        .unwrap();
    assert_eq!(side.upstream.as_deref(), Some("upstream/renamed"));
    assert!(!side.is_head);
    // Now it has an upstream: a plain push goes there.
    let pushed = t.engine.push("refs/heads/side".into(), false).await.unwrap();
    assert_eq!((pushed.remote.as_str(), pushed.upstream_set), ("upstream", false));
}

// ---- remote operations ----------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn fetch_without_remotes_is_an_empty_summary_and_emits_nothing() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    commit(&repo, &[("f.txt", "1\n")], "one");
    t.install_session(dir.path());
    let summary = t.engine.fetch(None).await.unwrap();
    assert!(summary.fetched.is_empty() && summary.failed.is_empty());
    assert!(t.sink.take().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn push_to_local_bare_then_fetch_from_it() {
    let t = test_engine();
    let r = with_remote();
    let ctx = t.install_session(r.work.workdir().unwrap());

    let pushed = t.engine.push("refs/heads/main".into(), false).await.unwrap();
    assert_eq!(pushed.remote, "origin");
    assert_eq!(pushed.remote_ref, "refs/heads/main");
    assert!(pushed.upstream_set);
    let events = t.sink.take();
    let progress = progress_events(&events);
    assert!(progress.iter().all(|p| p.op == RemoteOp::Push), "{progress:?}");
    assert!(events.iter().all(|e| e.repo() == Some(&ctx.repo_id)));
    let rest: Vec<&str> = events.iter().map(AppEvent::name).filter(|n| *n != "RemoteProgress").collect();
    assert_eq!(rest, REMOTE_REFS_EVENTS);

    // Nothing new on the remote: no ref changed, but the ref lists are still refreshed.
    let summary = t.engine.fetch(None).await.unwrap();
    assert!(!summary.updated_any());
    assert_eq!(names(&t), ALL_REFS_EVENTS);

    let (_other_dir, other) = other_clone(&r.bare_path);
    let theirs = commit(&other, &[("g.txt", "theirs\n")], "theirs");
    git2_push(&other, "refs/heads/main:refs/heads/main");

    let summary = t.engine.fetch(Some("origin".into())).await.unwrap();
    assert!(summary.updated_any());
    let events = t.sink.take();
    let progress = progress_events(&events);
    assert!(progress.iter().all(|p| p.op == RemoteOp::Fetch), "{progress:?}");
    // The local transport reports object counts: progress ends at 100 % exactly once.
    assert_eq!(progress.last().and_then(|p| p.percent), Some(100), "{progress:?}");
    assert_eq!(progress.iter().filter(|p| p.percent == Some(100)).count(), 1, "{progress:?}");
    let rest: Vec<&str> = events.iter().map(AppEvent::name).filter(|n| *n != "RemoteProgress").collect();
    assert_eq!(rest, ALL_REFS_EVENTS);
    assert_eq!(r.work.find_reference("refs/remotes/origin/main").unwrap().target(), Some(theirs));
    let main = &t.engine.branches(BranchKind::Local).await.unwrap()[0];
    assert_eq!((main.ahead, main.behind), (0, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_prunes_branches_deleted_on_the_remote() {
    let t = test_engine();
    let r = with_remote();
    t.install_session(r.work.workdir().unwrap());
    t.engine.push("refs/heads/main".into(), false).await.unwrap();
    let (_d, other) = other_clone(&r.bare_path);
    git2_push(&other, "refs/heads/main:refs/heads/gone");
    t.engine.fetch(None).await.unwrap();
    assert!(r.work.find_reference("refs/remotes/origin/gone").is_ok());

    git2_push(&other, ":refs/heads/gone");
    t.sink.take();
    let summary = t.engine.fetch(None).await.unwrap();
    assert!(summary.updated_any());
    assert!(r.work.find_reference("refs/remotes/origin/gone").is_err());
    let remote = t.engine.branches(BranchKind::Remote).await.unwrap();
    assert!(remote.iter().all(|b| !b.name.ends_with("gone")), "{remote:?}");
    assert!(names(&t).contains(&"RemoteBranchesUpdated"));
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_fast_forward() {
    let t = test_engine();
    let r = with_remote();
    t.install_session(r.work.workdir().unwrap());
    t.engine.push("refs/heads/main".into(), false).await.unwrap();
    let (_d, other) = other_clone(&r.bare_path);
    let theirs = commit(&other, &[("g.txt", "theirs\n")], "theirs");
    git2_push(&other, "refs/heads/main:refs/heads/main");
    t.sink.take();

    let outcome = t.engine.pull("refs/heads/main".into()).await.unwrap();
    assert!(matches!(outcome.merge, MergeOutcome::FastForward { to, .. } if to == oid(theirs)));
    assert!(!outcome.fetch.updated.is_empty());
    let events = t.sink.take();
    assert!(progress_events(&events).iter().all(|p| p.op == RemoteOp::Pull));
    let rest: Vec<&str> = events.iter().map(AppEvent::name).filter(|n| *n != "RemoteProgress").collect();
    assert_eq!(
        rest,
        [
            "CommitsUpdated",
            "CommitsHeadUpdated",
            "LocalBranchesUpdated",
            "RemoteBranchesUpdated",
            "RemotesUpdated",
            "GitFilesChanged",
            "StashesUpdated",
            "RepoStatusUpdated"
        ]
    );
    assert_eq!(read(&r.work, "g.txt"), "theirs\n");

    // Again: nothing fetched, up to date; the ref lists are still refreshed.
    let outcome = t.engine.pull("refs/heads/main".into()).await.unwrap();
    assert_eq!(outcome.merge, MergeOutcome::UpToDate);
    assert_eq!(names(&t), ALL_REFS_EVENTS);
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_with_conflicts_then_resolve_and_commit_the_merge() {
    let t = test_engine();
    set_signature(&t);
    let r = with_remote();
    t.install_session(r.work.workdir().unwrap());
    t.engine.push("refs/heads/main".into(), false).await.unwrap();
    let (_d, other) = other_clone(&r.bare_path);
    commit(&other, &[("f.txt", "one\nTHEIRS\nthree\n")], "theirs");
    git2_push(&other, "refs/heads/main:refs/heads/main");
    let ours = commit(&r.work, &[("f.txt", "one\nOURS\nthree\n")], "ours");
    t.sink.take();

    let outcome = t.engine.pull("refs/heads/main".into()).await.unwrap();
    assert_eq!(
        outcome.merge,
        MergeOutcome::Conflicts {
            paths: vec![PathBuf::from("f.txt")]
        }
    );
    let events = t.sink.take();
    // Conflicts: remote refs + HEAD-level refresh (files now conflicted).
    let files = events.iter().find_map(|e| match e {
        AppEvent::GitFilesChanged { files, .. } => Some(files.clone()),
        _ => None,
    });
    let files = files.expect("GitFilesChanged after a conflicting pull");
    assert_eq!(files.unstaged[0].change, DeltaChange::Conflicted);
    assert!(events.iter().any(|e| e.name() == "RemoteBranchesUpdated"));
    let status = events.iter().find_map(|e| match e {
        AppEvent::RepoStatusUpdated { status, .. } => Some(status.clone()),
        _ => None,
    });
    let status = status.expect("RepoStatusUpdated after a conflicting pull");
    assert!(status.is_merging(), "{status:?}");
    assert!(status.prepared_summary().is_some_and(|s| s.starts_with("Merge")), "{status:?}");
    assert_eq!(r.work.head().unwrap().target(), Some(ours), "HEAD did not move");

    let conflict = t.engine.merge_conflict("f.txt".into()).await.unwrap();
    assert!(conflict.ours.contains("OURS") && conflict.theirs.contains("THEIRS"));
    assert_eq!(conflict.chunks.len(), 1);
    assert!(t.sink.take().is_empty());

    t.engine
        .resolve_conflict("f.txt".into(), "one\nBOTH\nthree\n".into())
        .await
        .unwrap();
    assert_eq!(names(&t), FILES_EVENTS);
    let merge_commit = t.engine.commit_staged_files("Merge".into(), None).await.unwrap();
    assert_eq!(names(&t), HEAD_EVENTS);
    let c = r.work.find_commit(gityas_git::convert::oid_to_git(&merge_commit).unwrap()).unwrap();
    assert_eq!(c.parent_count(), 2);
    assert_eq!(r.work.state(), git2::RepositoryState::Clean);
}

/// Two local branches changing the same line.
fn conflicting_branches() -> (TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    let base = commit(&repo, &[("f.txt", "base\n"), ("keep.txt", "k\n")], "base");
    repo.branch("feature", &repo.find_commit(base).unwrap(), false).unwrap();
    commit(&repo, &[("f.txt", "main\n")], "main change");
    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force())).unwrap();
    commit(&repo, &[("f.txt", "feature\n"), ("added.txt", "new\n")], "feature change");
    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force())).unwrap();
    (dir, repo)
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_conflict_take_theirs_then_abort_restores() {
    let t = test_engine();
    let (dir, repo) = conflicting_branches();
    t.install_session(dir.path());

    // A real merge needs the signature; refused before anything is touched.
    assert!(matches!(
        t.engine.merge("refs/heads/feature".into(), "refs/heads/main".into()).await,
        Err(GitYasError::SignatureRequired(_))
    ));
    assert!(t.sink.take().is_empty());
    assert_eq!(repo.state(), git2::RepositoryState::Clean);
    set_signature(&t);

    let outcome = t.engine.merge("refs/heads/feature".into(), "refs/heads/main".into()).await.unwrap();
    assert_eq!(
        outcome,
        MergeOutcome::Conflicts {
            paths: vec![PathBuf::from("f.txt")]
        }
    );
    assert_eq!(names(&t), HEAD_EVENTS);
    assert_eq!(repo.state(), git2::RepositoryState::Merge);

    t.engine
        .resolve_conflict_with_action("f.txt".into(), DeletedMergeAction::TakeTheirs)
        .await
        .unwrap();
    assert_eq!(names(&t), FILES_EVENTS);
    assert_eq!(read(&repo, "f.txt"), "feature\n");

    let aborted = t.engine.abort_merge().await.unwrap();
    assert!(aborted.restored.contains(&PathBuf::from("f.txt")), "{aborted:?}");
    assert!(aborted.removed.contains(&PathBuf::from("added.txt")), "{aborted:?}");
    assert_eq!(names(&t), FILES_EVENTS);
    assert_eq!(read(&repo, "f.txt"), "main\n");
    assert_eq!(repo.state(), git2::RepositoryState::Clean);
    assert_eq!(t.engine.file_state().await.unwrap(), Default::default());

    // No merge in progress any more.
    assert!(t.engine.abort_merge().await.is_err());
    assert!(t.sink.take().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_fast_forward_and_merge_commit_signature_rules() {
    let t = test_engine();
    let (dir, repo) = conflicting_branches();
    // A fast-forwardable branch: `ff` one commit ahead of main.
    let main = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("ff", &main, false).unwrap();
    repo.set_head("refs/heads/ff").unwrap();
    let ahead = commit(&repo, &[("keep.txt", "k2\n")], "ahead");
    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force())).unwrap();
    t.install_session(dir.path());

    let outcome = t.engine.merge("refs/heads/ff".into(), "refs/heads/main".into()).await.unwrap();
    assert!(matches!(outcome, MergeOutcome::FastForward { to, .. } if to == oid(ahead)));
    assert_eq!(names(&t), HEAD_EVENTS);
    assert_eq!(
        t.engine.merge("refs/heads/ff".into(), "refs/heads/main".into()).await.unwrap(),
        MergeOutcome::UpToDate
    );
    assert!(t.sink.take().is_empty());
}

// ---- cancellation ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn remote_op_on_a_cancelled_session_is_cancelled() {
    let t = test_engine();
    let r = with_remote();
    t.install_session(r.work.workdir().unwrap());
    // What open_repository does before switching: cancel the session and its remote ops.
    t.engine.inner.sessions.cancel_active();
    assert_eq!(t.engine.fetch(None).await, Err(GitYasError::Cancelled));
    assert_eq!(t.engine.push("refs/heads/main".into(), false).await, Err(GitYasError::Cancelled));
    assert!(t.sink.take().is_empty());
    assert!(
        Repository::open_bare(&r.bare_path)
            .unwrap()
            .find_reference("refs/heads/main")
            .is_err()
    );
}

/// Calls `cancel_remote_operation` on the first progress event (from inside the libgit2 callback), so the
/// operation is deterministically in flight when it is cancelled.
struct CancelOnProgress {
    engine: OnceLock<Engine>,
    recorded: RecordingSink,
    cancelled: std::sync::atomic::AtomicUsize,
}

impl EventSink for CancelOnProgress {
    fn emit(&self, event: AppEvent) {
        if matches!(event, AppEvent::RemoteProgress { .. })
            && let Some(engine) = self.engine.get()
        {
            self.cancelled
                .fetch_add(engine.cancel_remote_operation(), std::sync::atomic::Ordering::SeqCst);
        }
        self.recorded.emit(event);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_remote_operation_aborts_an_in_flight_fetch() {
    let sink = Arc::new(CancelOnProgress {
        engine: OnceLock::new(),
        recorded: RecordingSink::new(),
        cancelled: Default::default(),
    });
    let engine = Engine::with_options(
        Storage::in_memory().unwrap(),
        Arc::new(MemoryCredentialStore::new()),
        sink.clone(),
        tokio::runtime::Handle::current(),
        EngineOptions {
            fetch_interval: None,
            ..EngineOptions::default()
        },
    );
    assert!(sink.engine.set(engine.clone()).is_ok());
    let t = TestEngine {
        engine,
        sink: Arc::new(RecordingSink::new()),
        credentials: Arc::new(MemoryCredentialStore::new()),
    };
    let r = with_remote();
    git2_push(&r.work, "refs/heads/main:refs/heads/main");
    let (_d, other) = other_clone(&r.bare_path);
    let many: Vec<(String, String)> = (0..200).map(|i| (format!("d/{i}.txt"), format!("content {i}\n"))).collect();
    let refs: Vec<(&str, &str)> = many.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    commit(&other, &refs, "many files");
    git2_push(&other, "refs/heads/main:refs/heads/main");
    t.install_session(r.work.workdir().unwrap());

    assert_eq!(t.engine.fetch(Some("origin".into())).await, Err(GitYasError::Cancelled));
    assert_eq!(sink.cancelled.load(std::sync::atomic::Ordering::SeqCst), 1, "one op was in flight");
    let before = r.work.find_reference("refs/remotes/origin/main").ok().and_then(|r| r.target());
    assert_ne!(before, other.head().unwrap().target(), "the fetch did not complete");
    let refreshes = sink.recorded.take().into_iter().filter(|e| e.name() != "RemoteProgress").count();
    assert_eq!(refreshes, 0);
    // The registry is clean again and a new fetch works.
    assert_eq!(t.engine.cancel_remote_operation(), 0);
}

// ---- stash ----------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn stash_files_stash_all_apply_pop_and_delete() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    commit(&repo, &[("a.txt", "a\n"), ("b.txt", "b\n")], "c");
    write(&repo, "a.txt", "a2\n");
    write(&repo, "b.txt", "b2\n");
    t.install_session(dir.path());

    assert!(matches!(
        t.engine.stash_all(None, true).await,
        Err(GitYasError::SignatureRequired(_))
    ));
    set_signature(&t);

    t.engine
        .stash_files(vec![modified("a.txt")], Some("only a".into()), true)
        .await
        .unwrap();
    assert_eq!(
        names(&t),
        ["CommitsUpdated", "GitFilesChanged", "StashesUpdated", "RepoStatusUpdated"]
    );
    assert_eq!(read(&repo, "a.txt"), "a\n");
    assert_eq!(read(&repo, "b.txt"), "b2\n", "unselected file untouched");

    t.engine.stash_all(Some("rest".into()), true).await.unwrap();
    assert_eq!(
        names(&t),
        ["CommitsUpdated", "GitFilesChanged", "StashesUpdated", "RepoStatusUpdated"]
    );
    assert_eq!(read(&repo, "b.txt"), "b\n");
    assert!(matches!(t.engine.stash_all(None, true).await, Err(GitYasError::InvalidInput(_))));
    assert!(t.sink.take().is_empty());

    let stashes = t.engine.stashes().await.unwrap();
    assert_eq!(stashes.len(), 2);
    assert!(stashes[0].stash.message.contains("rest"));
    assert!(stashes[1].stash.message.contains("only a"));
    assert!(t.sink.take().is_empty());

    assert_eq!(t.engine.apply_stash(0).await.unwrap(), StashApplyOutcome::Applied);
    assert_eq!(names(&t), FILES_EVENTS);
    assert_eq!(read(&repo, "b.txt"), "b2\n");
    assert_eq!(t.engine.stashes().await.unwrap().len(), 2, "apply keeps the stash");

    t.engine.delete_stash(0).await.unwrap();
    assert_eq!(names(&t), ["CommitsUpdated", "StashesUpdated"]);
    assert_eq!(t.engine.pop_stash(0).await.unwrap(), StashApplyOutcome::Applied);
    assert_eq!(
        names(&t),
        ["CommitsUpdated", "GitFilesChanged", "StashesUpdated", "RepoStatusUpdated"]
    );
    assert_eq!(read(&repo, "a.txt"), "a2\n");
    assert!(t.engine.stashes().await.unwrap().is_empty());
    assert!(matches!(t.engine.delete_stash(0).await, Err(GitYasError::NotFound(_))));
}

#[tokio::test(flavor = "multi_thread")]
async fn pop_with_conflicts_keeps_the_stash() {
    let t = test_engine();
    set_signature(&t);
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    commit(&repo, &[("f.txt", "base\n")], "c");
    write(&repo, "f.txt", "stashed\n");
    t.install_session(dir.path());
    t.engine.stash_all(None, false).await.unwrap();
    commit(&repo, &[("f.txt", "committed\n")], "conflicting");
    t.sink.take();

    let outcome = t.engine.pop_stash(0).await.unwrap();
    assert_eq!(
        outcome,
        StashApplyOutcome::Conflicts {
            paths: vec![PathBuf::from("f.txt")]
        }
    );
    assert_eq!(names(&t), FILES_EVENTS, "stash kept: refs/stash unchanged");
    assert_eq!(t.engine.stashes().await.unwrap().len(), 1);
    let state = t.engine.file_state().await.unwrap();
    assert_eq!(state.unstaged[0].change, DeltaChange::Conflicted);
}

// ---- progress mapping -----------------------------------------------------------------------------------------

fn fetch_progress(received: usize, total: usize, indexed_deltas: usize, total_deltas: usize) -> TransferProgress {
    TransferProgress::Fetch {
        received_objects: received,
        total_objects: total,
        indexed_objects: received,
        indexed_deltas,
        total_deltas,
        received_bytes: 0,
    }
}

#[test]
fn describe_maps_fetch_push_and_sideband() {
    assert_eq!(describe(&fetch_progress(0, 0, 0, 0)).unwrap().0, None);
    assert_eq!(
        describe(&fetch_progress(5, 10, 0, 0)).unwrap(),
        (Some(45), "Receiving objects: 5/10".into())
    );
    assert_eq!(
        describe(&fetch_progress(10, 10, 5, 10)).unwrap(),
        (Some(95), "Resolving deltas: 5/10".into())
    );
    assert_eq!(describe(&fetch_progress(10, 10, 10, 10)).unwrap().0, Some(100));
    assert_eq!(describe(&fetch_progress(10, 10, 0, 0)).unwrap().0, Some(100));
    let push = TransferProgress::Push {
        current: 1,
        total: 4,
        bytes: 0,
    };
    assert_eq!(describe(&push).unwrap(), (Some(25), "Writing objects: 1/4".into()));
    let push0 = TransferProgress::Push {
        current: 0,
        total: 0,
        bytes: 0,
    };
    assert_eq!(describe(&push0).unwrap().0, None);
    assert_eq!(
        describe(&TransferProgress::Sideband("Counting: 10%\rCounting: 20%\r".into())).unwrap(),
        (None, "Counting: 20%".into())
    );
    assert_eq!(describe(&TransferProgress::Sideband(" \r\n".into())), None);
    // Overflowing counts are clamped.
    assert_eq!(describe(&fetch_progress(20, 10, 0, 0)).unwrap().0, Some(100));
}

#[test]
fn reporter_throttles_is_monotonic_and_always_ends_at_100() {
    let out = std::cell::RefCell::new(Vec::<RemoteProgress>::new());
    let reporter = ProgressReporter::new(RemoteOp::Fetch, |p| out.borrow_mut().push(p));
    for i in 0..1000 {
        reporter.report(&fetch_progress(i, 1000, 0, 0));
    }
    // First report goes out immediately, the rest within MIN_INTERVAL are throttled.
    assert!(out.borrow().len() < 10, "{} events", out.borrow().len());
    // Completing (100 %) is never throttled, and only emitted once.
    reporter.report(&fetch_progress(1000, 1000, 0, 0));
    reporter.report(&fetch_progress(1000, 1000, 0, 0));
    assert_eq!(out.borrow().last().unwrap().percent, Some(100));
    // libgit2 learns about deltas late: the percent must not go back.
    std::thread::sleep(MIN_INTERVAL);
    reporter.report(&fetch_progress(1000, 1000, 1, 10));
    assert_eq!(out.borrow().last().unwrap().percent, Some(100));
    let count = out.borrow().len();
    reporter.finish();
    assert_eq!(out.borrow().len(), count, "100 % already emitted");
    let percents: Vec<Option<u8>> = out.borrow().iter().map(|p| p.percent).collect();
    assert!(percents.windows(2).all(|w| w[0] <= w[1]), "{percents:?}");
}

#[test]
fn reporter_finish_emits_final_100_only_when_a_total_was_known() {
    let out = std::cell::RefCell::new(Vec::<RemoteProgress>::new());
    let reporter = ProgressReporter::new(RemoteOp::Push, |p| out.borrow_mut().push(p));
    reporter.report(&TransferProgress::Push {
        current: 1,
        total: 3,
        bytes: 0,
    });
    reporter.report(&TransferProgress::Sideband("remote: hello".into()));
    reporter.finish();
    let got = out.borrow().clone();
    assert_eq!(got.first().unwrap().percent, Some(33));
    assert_eq!(
        got.last().unwrap(),
        &RemoteProgress {
            op: RemoteOp::Push,
            percent: Some(100),
            message: "Done".into()
        }
    );

    let out = std::cell::RefCell::new(Vec::<RemoteProgress>::new());
    let reporter = ProgressReporter::new(RemoteOp::Fetch, |p| out.borrow_mut().push(p));
    reporter.report(&TransferProgress::Sideband("remote: counting".into()));
    reporter.finish();
    assert_eq!(out.borrow().len(), 1, "no total ever known: no fabricated 100 %");
    assert_eq!(out.borrow()[0].percent, None);
}

// ---- repository states ----------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn bare_repository_working_tree_commands_fail_cleanly() {
    let t = test_engine();
    let r = with_remote();
    git2_push(&r.work, "refs/heads/main:refs/heads/main");
    t.install_session(&r.bare_path);
    assert!(t.engine.head().await.unwrap().is_some_and(|h| h.branch_name() == Some("main")));
    assert_eq!(t.engine.branches(BranchKind::Local).await.unwrap().len(), 1);
    assert!(matches!(t.engine.file_state().await, Err(GitYasError::InvalidInput(_))));
    assert!(t.engine.stage_files(vec![modified("f.txt")]).await.is_err());
    assert!(t.engine.discard_files(vec![modified("f.txt")]).await.is_err());
    assert!(t.sink.take().is_empty());
}
