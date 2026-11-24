//! Watcher and scheduler tests against temp repositories. Filesystem events are asynchronous and their latency
//! varies, so every positive assertion polls with a timeout ([`wait_for`]); negative assertions ("no event") watch
//! for a fixed quiet period after a positive control proved the watcher is live ([`ready`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gityas_core::AppEvent;
use tokio::task::JoinHandle;

use super::watcher::{self, Backend};
use super::{scheduler, start_session_tasks};
use crate::EngineOptions;
use crate::session::SessionCtx;
use crate::testutil::{TestEngine, test_engine, test_engine_with};

const TIMEOUT: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_millis(600);

/// Backend used by these tests (and by `start_session_tasks` under `cfg(test)`). Defaults to notify's
/// `PollWatcher`: inside the development sandbox FSEvents accepts the watch but never delivers an event.
/// Set `GITYAS_TEST_NATIVE_WATCHER=1` to run the same tests against the native backend.
pub(super) fn backend() -> Backend {
    if std::env::var_os("GITYAS_TEST_NATIVE_WATCHER").is_some() {
        Backend::Native
    } else {
        Backend::Poll(Duration::from_millis(50))
    }
}

/// Polls the sink until `done(all events so far)` or the timeout; returns every event seen.
async fn wait_for(t: &TestEngine, done: impl Fn(&[AppEvent]) -> bool) -> Vec<AppEvent> {
    let start = Instant::now();
    let mut seen = Vec::new();
    loop {
        seen.extend(t.sink.take());
        if done(&seen) || start.elapsed() > TIMEOUT {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Collects events for `period`.
async fn collect(t: &TestEngine, period: Duration) -> Vec<AppEvent> {
    tokio::time::sleep(period).await;
    t.sink.take()
}

/// Drains events until a whole [`QUIET`] period passes without any (or the timeout).
async fn settle(t: &TestEngine) {
    let start = Instant::now();
    while !collect(t, QUIET).await.is_empty() && start.elapsed() < TIMEOUT {}
}

fn names(events: &[AppEvent]) -> Vec<&'static str> {
    events.iter().map(AppEvent::name).collect()
}

fn has(events: &[AppEvent], name: &str) -> bool {
    events.iter().any(|e| e.name() == name)
}

fn count(events: &[AppEvent], name: &str) -> usize {
    events.iter().filter(|e| e.name() == name).count()
}

/// A repository with one commit (`README`) on `main`.
fn repo_with_commit(dir: &Path) -> git2::Repository {
    let repo = git2::Repository::init_opts(dir, git2::RepositoryInitOptions::new().initial_head("main")).unwrap();
    std::fs::write(dir.join("README"), "hello\n").unwrap();
    commit_all(&repo, "initial");
    repo
}

fn sig() -> git2::Signature<'static> {
    git2::Signature::now("T", "t@example.com").unwrap()
}

fn commit_all(repo: &git2::Repository, message: &str) -> git2::Oid {
    let mut index = repo.index().unwrap();
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    repo.commit(Some("HEAD"), &sig(), &sig(), message, &tree, &parents).unwrap()
}

/// Starts the watcher and proves it delivers events: rewrites `HEAD` (same content) until a refresh shows up, then
/// drains the sink. `gitdir` is the session's per-worktree git dir.
async fn start_watcher(t: &TestEngine, ctx: &SessionCtx, gitdir: &Path) -> JoinHandle<()> {
    let task = watcher::spawn(&t.engine.inner, ctx, backend());
    ready(t, gitdir).await;
    task
}

async fn ready(t: &TestEngine, gitdir: &Path) {
    let head = gitdir.join("HEAD");
    let content = std::fs::read(&head).unwrap();
    let start = Instant::now();
    loop {
        std::fs::write(&head, &content).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        if has(&t.sink.take(), "CommitsHeadUpdated") {
            break;
        }
        assert!(start.elapsed() < TIMEOUT, "watcher never delivered an event");
    }
    // Let the probe's tail settle, then start from a clean slate.
    collect(t, Duration::from_millis(200)).await;
}

async fn finishes(task: JoinHandle<()>) -> bool {
    tokio::time::timeout(TIMEOUT, task).await.is_ok_and(|r| r.is_ok())
}

#[tokio::test(flavor = "multi_thread")]
async fn working_tree_change_emits_git_files_changed() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    repo_with_commit(dir.path());
    let ctx = t.install_session(dir.path());
    let task = start_watcher(&t, &ctx, &dir.path().join(".git")).await;

    std::fs::write(dir.path().join("README"), "changed\n").unwrap();
    let events = wait_for(&t, |e| has(e, "GitFilesChanged")).await;
    let files = events.iter().find_map(|e| match e {
        AppEvent::GitFilesChanged { repo, files } => Some((repo.clone(), files.clone())),
        _ => None,
    });
    let (repo, files) = files.unwrap_or_else(|| panic!("no GitFilesChanged in {:?}", names(&events)));
    assert_eq!(repo, ctx.repo_id);
    assert_eq!(files.unstaged.len(), 1, "{files:?}");
    // A working-tree edit refreshes files only.
    assert_eq!(
        names(&events)
            .iter()
            .filter(|n| !matches!(**n, "GitFilesChanged" | "RepoStatusUpdated"))
            .count(),
        0,
        "{:?}",
        names(&events)
    );

    ctx.cancel.cancel();
    assert!(finishes(task).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_is_one_coalesced_refresh() {
    let t = test_engine_with(EngineOptions {
        fetch_interval: None,
        watch_debounce: Duration::from_millis(200),
        ..EngineOptions::default()
    });
    let dir = tempfile::tempdir().unwrap();
    let repo = repo_with_commit(dir.path());
    let ctx = t.install_session(dir.path());
    let task = start_watcher(&t, &ctx, &dir.path().join(".git")).await;

    std::fs::write(dir.path().join("new.txt"), "x\n").unwrap();
    let oid = commit_all(&repo, "second");
    let mut events = wait_for(&t, |e| {
        e.iter().any(|e| matches!(e, AppEvent::CommitsHeadUpdated { head: Some(h), .. } if h.target().map(ToString::to_string) == Some(oid.to_string())))
            && has(e, "CommitsUpdated")
    })
    .await;
    events.extend(collect(&t, QUIET).await);
    let n = names(&events);
    assert_eq!(count(&events, "CommitsUpdated"), 1, "{n:?}");
    assert_eq!(count(&events, "CommitsHeadUpdated"), 1, "{n:?}");
    assert_eq!(count(&events, "LocalBranchesUpdated"), 1, "{n:?}");
    assert!(has(&events, "GitFilesChanged"), "index changed: {n:?}");
    assert!(count(&events, "GitFilesChanged") <= 2, "{n:?}");

    ctx.cancel.cancel();
    assert!(finishes(task).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn branch_and_tag_creation_emit_local_branches() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let repo = repo_with_commit(dir.path());
    let ctx = t.install_session(dir.path());
    let task = start_watcher(&t, &ctx, &dir.path().join(".git")).await;

    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature", &head, false).unwrap();
    let events = wait_for(&t, |e| {
        e.iter()
            .any(|e| matches!(e, AppEvent::LocalBranchesUpdated { branches, .. } if branches.len() == 2))
    })
    .await;
    assert!(has(&events, "CommitsUpdated"), "{:?}", names(&events));
    assert!(!has(&events, "RemoteBranchesUpdated"), "{:?}", names(&events));

    repo.tag_lightweight("v1", head.as_object(), false).unwrap();
    let events = wait_for(&t, |e| {
        e.iter()
            .any(|e| matches!(e, AppEvent::CommitsUpdated { commits, .. } if commits.tags.iter().any(|t| t.name == "v1")))
    })
    .await;
    assert!(has(&events, "CommitsUpdated"), "{:?}", names(&events));

    ctx.cancel.cancel();
    assert!(finishes(task).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_tracking_ref_update_emits_remote_branches() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let repo = repo_with_commit(dir.path());
    let ctx = t.install_session(dir.path());
    let task = start_watcher(&t, &ctx, &dir.path().join(".git")).await;

    let head = repo.head().unwrap().target().unwrap();
    repo.reference("refs/remotes/origin/main", head, false, "test").unwrap();
    // Both events come from one refresh but can straddle a poll of the sink.
    let events = wait_for(&t, |e| {
        e.iter()
            .any(|e| matches!(e, AppEvent::RemoteBranchesUpdated { branches, .. } if branches.len() == 1))
            && has(e, "RemotesUpdated")
    })
    .await;
    assert!(has(&events, "RemotesUpdated"), "{:?}", names(&events));
    assert!(!has(&events, "GitFilesChanged"), "{:?}", names(&events));

    ctx.cancel.cancel();
    assert!(finishes(task).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn ignored_paths_and_git_internals_emit_nothing() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".gitignore"), "target/\nnode_modules/\n*.log\n").unwrap();
    let repo = repo_with_commit(dir.path());
    let ctx = t.install_session(dir.path());
    let task = start_watcher(&t, &ctx, &dir.path().join(".git")).await;

    for sub in ["target/debug/deps", "node_modules/a/lib"] {
        std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        for i in 0..50 {
            std::fs::write(dir.path().join(sub).join(format!("f{i}")), "x").unwrap();
        }
    }
    std::fs::write(dir.path().join("build.log"), "x").unwrap();
    // Git internals that are noise: objects, logs, lock files.
    repo.blob(b"loose object").unwrap();
    std::fs::write(dir.path().join(".git/logs/HEAD.extra"), "x").unwrap();
    std::fs::write(dir.path().join(".git/index.lock.tmp.lock"), "x").unwrap();
    // A nested repository's git dir (untracked nested repo / submodule-like).
    let nested = dir.path().join("vendor/nested");
    git2::Repository::init(&nested).unwrap();
    // Creating the nested repo's working dir itself is a (non-ignored) working-tree change; let that burst finish,
    // however slow the machine (a fixed 400 ms was too short on CI macOS runners).
    settle(&t).await;
    std::fs::write(nested.join(".git/HEAD"), "ref: refs/heads/other\n").unwrap();
    std::fs::write(nested.join(".git/description"), "x").unwrap();

    let events = collect(&t, QUIET).await;
    assert!(events.is_empty(), "{:?}", names(&events));

    // Positive control: a tracked file still refreshes.
    std::fs::write(dir.path().join("README"), "changed\n").unwrap();
    let events = wait_for(&t, |e| has(e, "GitFilesChanged")).await;
    assert!(has(&events, "GitFilesChanged"));

    ctx.cancel.cancel();
    assert!(finishes(task).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_session_stops_tasks_and_emits_nothing() {
    let t = test_engine_with(EngineOptions {
        fetch_interval: Some(Duration::from_millis(50)),
        watch_debounce: Duration::from_millis(50),
        ..EngineOptions::default()
    });
    let dir = tempfile::tempdir().unwrap();
    repo_with_commit(dir.path());
    let ctx = t.install_session(dir.path());
    let tasks = start_session_tasks(&t.engine.inner, &ctx).unwrap();
    assert_eq!(tasks.len(), 2, "watcher + scheduler");
    ready(&t, &dir.path().join(".git")).await;

    let started = Instant::now();
    t.engine.inner.sessions.cancel_active();
    for task in tasks {
        assert!(finishes(task).await, "task did not stop");
    }
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());

    std::fs::write(dir.path().join("README"), "changed\n").unwrap();
    let events = collect(&t, QUIET).await;
    assert!(events.is_empty(), "{:?}", names(&events));
}

#[tokio::test(flavor = "multi_thread")]
async fn replaced_session_watcher_goes_quiet() {
    // A switch installs a new session; the old watcher must not emit for its repository any more.
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    repo_with_commit(dir.path());
    let old = t.install_session(dir.path());
    let task = start_watcher(&t, &old, &dir.path().join(".git")).await;

    let other = tempfile::tempdir().unwrap();
    repo_with_commit(other.path());
    t.install_session(other.path()); // cancels `old`
    assert!(finishes(task).await);
    std::fs::write(dir.path().join("README"), "changed\n").unwrap();
    let events = collect(&t, QUIET).await;
    assert!(events.is_empty(), "{:?}", names(&events));
}

#[tokio::test(flavor = "multi_thread")]
async fn linked_worktree_watches_its_gitdir_and_the_shared_refs() {
    let t = test_engine();
    let main_dir = tempfile::tempdir().unwrap();
    let main = repo_with_commit(main_dir.path());
    let wt_parent = tempfile::tempdir().unwrap();
    let wt_path = wt_parent.path().join("wt");
    main.worktree("wt", &wt_path, None).unwrap();
    let wt = git2::Repository::open(&wt_path).unwrap();
    assert!(wt.is_worktree());
    let ctx = t.install_session(&wt_path);
    let task = start_watcher(&t, &ctx, wt.path()).await;

    // Working tree of the linked worktree.
    std::fs::write(wt_path.join("README"), "changed in wt\n").unwrap();
    let events = wait_for(&t, |e| has(e, "GitFilesChanged")).await;
    assert!(has(&events, "GitFilesChanged"), "{:?}", names(&events));

    // A branch created anywhere lives in the common dir.
    let head = main.head().unwrap().peel_to_commit().unwrap();
    main.branch("from-main", &head, false).unwrap();
    let events = wait_for(&t, |e| {
        e.iter()
            .any(|e| matches!(e, AppEvent::LocalBranchesUpdated { branches, .. } if branches.iter().any(|b| b.name == "from-main")))
    })
    .await;
    assert!(has(&events, "LocalBranchesUpdated"), "{:?}", names(&events));

    // The main worktree's files are not this session's; its HEAD only updates the worktree list.
    collect(&t, Duration::from_millis(200)).await;
    std::fs::write(main_dir.path().join("README"), "changed in main\n").unwrap();
    let main_head = main_dir.path().join(".git/HEAD");
    std::fs::write(&main_head, std::fs::read(&main_head).unwrap()).unwrap();
    let events = collect(&t, QUIET).await;
    assert!(events.iter().all(|e| e.name() == "WorktreesUpdated"), "{:?}", names(&events));

    ctx.cancel.cancel();
    assert!(finishes(task).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn bare_repository_refreshes_refs_but_not_files() {
    let t = test_engine();
    let src = tempfile::tempdir().unwrap();
    repo_with_commit(src.path());
    let bare_dir = tempfile::tempdir().unwrap();
    let bare = git2::build::RepoBuilder::new()
        .bare(true)
        .clone(src.path().to_str().unwrap(), bare_dir.path())
        .unwrap();
    let ctx = t.install_session(bare_dir.path());
    let task = start_watcher(&t, &ctx, bare_dir.path()).await;

    let head = bare.head().unwrap().peel_to_commit().unwrap();
    bare.branch("feature", &head, false).unwrap();
    let mut events = wait_for(&t, |e| has(e, "LocalBranchesUpdated")).await;
    events.extend(collect(&t, Duration::from_millis(200)).await);
    assert!(has(&events, "LocalBranchesUpdated"), "{:?}", names(&events));
    assert!(!has(&events, "GitFilesChanged"), "bare: no status refresh: {:?}", names(&events));

    ctx.cancel.cancel();
    assert!(finishes(task).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn deleted_repository_warns_once_and_stops() {
    let t = test_engine();
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("repo");
    repo_with_commit(&dir);
    let ctx = t.install_session(&dir);
    let task = start_watcher(&t, &ctx, &dir.join(".git")).await;

    std::fs::remove_dir_all(&dir).unwrap();
    assert!(finishes(task).await, "watcher kept running after the repository was deleted");
    let events = t.sink.take();
    let warnings = count(&events, "Warning");
    assert_eq!(warnings, 1, "{events:?}");
    assert!(collect(&t, QUIET).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_that_cannot_start_warns_and_the_session_still_works() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("repo");
    repo_with_commit(&path);
    let ctx = t.install_session(&path);
    std::fs::remove_dir_all(&path).unwrap();
    let task = watcher::spawn(&t.engine.inner, &ctx, backend());
    assert!(finishes(task).await);
    let events = t.sink.take();
    assert!(
        matches!(events.as_slice(), [AppEvent::Warning(w)] if w.contains("Live updates are off")),
        "{events:?}"
    );
    assert!(t.engine.inner.sessions.is_current(&ctx), "the session is not torn down");
}

/// `src` with a bare `remote` as `origin`, fetched once; returns (local, remote bare repo).
fn repo_with_remote(local: &Path, remote: &Path) -> (git2::Repository, git2::Repository) {
    let seed = tempfile::tempdir().unwrap();
    let seed_repo = repo_with_commit(seed.path());
    drop(seed_repo);
    let bare = git2::build::RepoBuilder::new()
        .bare(true)
        .clone(seed.path().to_str().unwrap(), remote)
        .unwrap();
    let local_repo = git2::build::RepoBuilder::new().clone(remote.to_str().unwrap(), local).unwrap();
    (local_repo, bare)
}

/// A new commit on `main` in the bare remote, made directly with git2.
fn advance_remote(bare: &git2::Repository) -> git2::Oid {
    let parent = bare.find_reference("refs/heads/main").unwrap().peel_to_commit().unwrap();
    let mut tb = bare.treebuilder(Some(&parent.tree().unwrap())).unwrap();
    tb.insert("remote.txt", bare.blob(b"from remote\n").unwrap(), 0o100644).unwrap();
    let tree = bare.find_tree(tb.write().unwrap()).unwrap();
    bare.commit(Some("refs/heads/main"), &sig(), &sig(), "remote commit", &tree, &[&parent])
        .unwrap()
}

fn remote_main(events: &[AppEvent]) -> Option<String> {
    events.iter().rev().find_map(|e| match e {
        AppEvent::RemoteBranchesUpdated { branches, .. } => branches
            .iter()
            .find(|b| b.name == "origin/main")
            .and_then(|b| b.target.as_ref().map(ToString::to_string)),
        _ => None,
    })
}

/// Starts a scheduler with the interval set to `period` (as a settings change would).
fn spawn_every(t: &TestEngine, ctx: &SessionCtx, period: Duration) -> tokio::task::JoinHandle<()> {
    t.engine.inner.fetch_interval.send_replace(Some(period));
    scheduler::spawn(&t.engine.inner, ctx)
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_follows_a_changed_interval() {
    let t = test_engine();
    let (local, remote) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (_local, bare) = repo_with_remote(local.path(), remote.path());
    let ctx = t.install_session(local.path());
    advance_remote(&bare);
    let task = spawn_every(&t, &ctx, Duration::from_secs(3600));
    assert!(collect(&t, Duration::from_millis(300)).await.is_empty());
    t.engine.inner.fetch_interval.send_replace(Some(Duration::from_millis(50)));
    let events = wait_for(&t, |e| has(e, "RemoteBranchesUpdated")).await;
    assert!(has(&events, "RemoteBranchesUpdated"), "{:?}", names(&events));
    ctx.cancel.cancel();
    assert!(finishes(task).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_fetches_remote_changes_and_emits_remote_refs() {
    let t = test_engine();
    let (local, remote) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (_local, bare) = repo_with_remote(local.path(), remote.path());
    let ctx = t.install_session(local.path());
    let oid = advance_remote(&bare);

    let task = spawn_every(&t, &ctx, Duration::from_millis(100));
    let events = wait_for(&t, |e| remote_main(e).as_deref() == Some(oid.to_string().as_str())).await;
    assert_eq!(remote_main(&events), Some(oid.to_string()), "{:?}", names(&events));
    for name in ["CommitsUpdated", "LocalBranchesUpdated", "RemotesUpdated"] {
        assert!(has(&events, name), "{name} missing: {:?}", names(&events));
    }
    // Nothing new on the remote: further ticks emit nothing.
    assert!(collect(&t, Duration::from_millis(400)).await.is_empty());

    ctx.cancel.cancel();
    assert!(finishes(task).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_first_tick_waits_one_interval() {
    let t = test_engine();
    let (local, remote) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (_local, bare) = repo_with_remote(local.path(), remote.path());
    let ctx = t.install_session(local.path());
    advance_remote(&bare);
    let task = spawn_every(&t, &ctx, Duration::from_secs(1));
    assert!(
        collect(&t, Duration::from_millis(500)).await.is_empty(),
        "fetched before the first interval"
    );
    let events = wait_for(&t, |e| has(e, "RemoteBranchesUpdated")).await;
    assert!(has(&events, "RemoteBranchesUpdated"), "{:?}", names(&events));
    ctx.cancel.cancel();
    assert!(finishes(task).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_without_remotes_keeps_running_and_emits_nothing() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    repo_with_commit(dir.path());
    let ctx = t.install_session(dir.path());
    let task = spawn_every(&t, &ctx, Duration::from_millis(30));
    let events = collect(&t, Duration::from_millis(400)).await;
    assert!(events.is_empty(), "{:?}", names(&events));
    assert!(!task.is_finished(), "scheduler died on a repository without remotes");
    ctx.cancel.cancel();
    assert!(finishes(task).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_logs_unreachable_remote_and_keeps_going() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let repo = repo_with_commit(dir.path());
    repo.remote("origin", dir.path().join("does-not-exist").to_str().unwrap()).unwrap();
    let ctx = t.install_session(dir.path());
    let task = spawn_every(&t, &ctx, Duration::from_millis(30));
    let events = collect(&t, Duration::from_millis(400)).await;
    assert!(events.is_empty(), "a failing fetch is logged, not emitted: {:?}", names(&events));
    assert!(!task.is_finished());
    ctx.cancel.cancel();
    assert!(finishes(task).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_skips_tick_while_a_remote_op_is_in_flight() {
    let t = test_engine();
    let (local, remote) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (_local, bare) = repo_with_remote(local.path(), remote.path());
    let ctx = t.install_session(local.path());
    advance_remote(&bare);
    let user_fetch = t.engine.inner.sessions.begin_remote_op(Some(&ctx));
    let task = spawn_every(&t, &ctx, Duration::from_millis(30));
    assert!(collect(&t, Duration::from_millis(300)).await.is_empty());
    drop(user_fetch);
    let events = wait_for(&t, |e| has(e, "RemoteBranchesUpdated")).await;
    assert!(has(&events, "RemoteBranchesUpdated"), "{:?}", names(&events));
    ctx.cancel.cancel();
    assert!(finishes(task).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn start_session_tasks_without_scheduler() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    repo_with_commit(dir.path());
    let ctx = t.install_session(dir.path());
    let tasks = start_session_tasks(&t.engine.inner, &ctx).unwrap();
    assert_eq!(tasks.len(), 1, "fetch_interval None: watcher only");
    ctx.cancel.cancel();
    for task in tasks {
        assert!(finishes(task).await);
    }
    let _: &Arc<crate::Inner> = &t.engine.inner;
}

/// Cost of classifying a `node_modules` storm (run with `--ignored --nocapture`).
#[test]
#[ignore = "benchmark"]
fn bench_classify_ignored_storm() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
    let repo = repo_with_commit(dir.path());
    let layout = watcher::Layout::of(&repo).unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let paths: Vec<PathBuf> = (0..200)
        .flat_map(|d| (0..100).map(move |f| (d, f)))
        .map(|(d, f)| root.join(format!("node_modules/pkg{d}/lib/file{f}.js")))
        .collect();
    for cached in [true, false] {
        let start = Instant::now();
        let mut ignore = watcher::IgnoreCheck::new(Some(git2::Repository::open(dir.path()).unwrap()));
        let mut hits = 0;
        for p in &paths {
            if cached {
                hits += usize::from(layout.classify(p, &mut ignore).is_some());
            } else {
                let rel = p.strip_prefix(&root).unwrap();
                hits += usize::from(!repo.is_path_ignored(rel).unwrap());
            }
        }
        assert_eq!(hits, 0);
        println!("{} paths, cached={cached}: {:?}", paths.len(), start.elapsed());
    }
}

/// A tab switch holds the command lock while it awaits the old session's tasks (`Session::stop`): a task waiting
/// for the lock must give up as soon as its session is cancelled, or the switch deadlocks.
#[tokio::test(flavor = "multi_thread")]
async fn tasks_waiting_for_the_command_lock_exit_on_cancel() {
    let t = test_engine();
    let (local, remote) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (_local, bare) = repo_with_remote(local.path(), remote.path());
    advance_remote(&bare);
    let ctx = t.install_session(local.path());
    let watcher = start_watcher(&t, &ctx, &local.path().join(".git")).await;
    let scheduler = spawn_every(&t, &ctx, Duration::from_millis(20));

    let guard = t.engine.inner.sessions.command_lock.lock().await;
    // Both tasks now block on the lock: the scheduler on its next tick, the watcher on this change.
    std::fs::write(local.path().join("README"), "changed\n").unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!watcher.is_finished() && !scheduler.is_finished());

    t.engine.inner.sessions.cancel_active();
    let started = Instant::now();
    assert!(finishes(watcher).await, "watcher stuck on the command lock");
    assert!(finishes(scheduler).await, "scheduler stuck on the command lock");
    assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
    drop(guard);
    assert!(collect(&t, QUIET).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_uses_the_sessions_current_provider() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    repo_with_commit(dir.path());
    let ctx = t.install_session(dir.path());
    assert_eq!(ctx.provider, None);
    // What `set_repository_provider` does: same session, new provider.
    let mut updated = ctx.clone();
    updated.provider = Some(gityas_core::Provider::Github);
    let sessions = &t.engine.inner.sessions;
    drop(sessions.replace(Some(crate::session::Session {
        ctx: updated,
        tasks: Vec::new(),
    })));
    assert_eq!(
        scheduler::current_provider(&t.engine.inner, &ctx),
        Some(gityas_core::Provider::Github)
    );
    // A stale snapshot (another session active) falls back to its own provider; `with_session_repo` refuses it anyway.
    let other = tempfile::tempdir().unwrap();
    repo_with_commit(other.path());
    t.install_session(other.path());
    assert_eq!(scheduler::current_provider(&t.engine.inner, &ctx), None);
}
