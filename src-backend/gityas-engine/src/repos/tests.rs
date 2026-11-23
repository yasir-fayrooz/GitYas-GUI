use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gityas_auth::{CredentialStore, MemoryCredentialStore};
use gityas_core::events::RecordingSink;
use gityas_core::models::{OpenTabs, RemoteOp};
use gityas_core::{AppEvent, Credential, EventSink, GitYasError, Provider};
use gityas_storage::{Storage, StoragePaths};

use crate::testutil::{TestEngine, file_url, test_engine};
use crate::{Engine, EngineOptions};

// ---- fixtures -------------------------------------------------------------------------------------------------

fn options() -> EngineOptions {
    EngineOptions {
        fetch_interval: None,
        watch_debounce: std::time::Duration::from_millis(50),
        ..EngineOptions::default()
    }
}

/// The form the engine registers (Windows `\\?\` prefix stripped).
fn canonical(path: &Path) -> PathBuf {
    super::register::canonicalize(path).unwrap()
}

fn commit_file(repo: &git2::Repository, name: &str, content: &str) -> git2::Oid {
    let workdir = repo.workdir().unwrap();
    std::fs::write(workdir.join(name), content).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new(name)).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    let parents: Vec<git2::Commit<'_>> = repo.head().ok().and_then(|h| h.peel_to_commit().ok()).into_iter().collect();
    let parents: Vec<&git2::Commit<'_>> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, &format!("add {name}"), &tree, &parents)
        .unwrap()
}

/// A working repository with one commit, optionally with an `origin` remote.
fn repo_with_commit(dir: &Path, origin: Option<&str>) -> git2::Repository {
    let repo = git2::Repository::init(dir).unwrap();
    repo.config().unwrap().set_bool("core.autocrlf", false).unwrap();
    commit_file(&repo, "README.md", "hello\n");
    if let Some(url) = origin {
        repo.remote("origin", url).unwrap();
    }
    repo
}

/// A bare repository with `commits` commits on `main`, to clone from.
fn bare_remote(dir: &Path, commits: usize) -> PathBuf {
    let work = dir.join("work");
    let repo = git2::Repository::init(&work).unwrap();
    repo.config().unwrap().set_bool("core.autocrlf", false).unwrap();
    for i in 0..commits {
        // Some content so a clone transfers a few objects.
        let content: String = (0..200).map(|j| format!("line {i} {j}\n")).collect();
        commit_file(&repo, &format!("file{i}.txt"), &content);
    }
    let bare = dir.join("remote.git");
    git2::Repository::init_bare(&bare).unwrap();
    let mut remote = repo.remote("origin", bare.to_str().unwrap()).unwrap();
    let head = repo.head().unwrap();
    let branch = head.name().unwrap().to_owned();
    remote.push(&[format!("{branch}:{branch}")], None).unwrap();
    let bare_repo = git2::Repository::open_bare(&bare).unwrap();
    bare_repo.set_head(&branch).unwrap();
    bare
}

fn names(events: &[AppEvent]) -> Vec<&'static str> {
    events.iter().map(AppEvent::name).collect()
}

fn disk_storage(dir: &Path) -> Storage {
    let (storage, report) = Storage::open(&StoragePaths {
        data_dir: dir.join("data"),
        cache_dir: dir.join("cache"),
    })
    .unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    storage
}

fn engine_on(storage: Storage, credentials: Arc<MemoryCredentialStore>) -> (Engine, Arc<RecordingSink>) {
    let sink = Arc::new(RecordingSink::new());
    let engine = Engine::with_options(storage, credentials, sink.clone(), tokio::runtime::Handle::current(), options());
    (engine, sink)
}

async fn add(t: &TestEngine, path: &Path) -> gityas_core::models::Repository {
    t.engine.add_repository(path.to_path_buf(), None).await.unwrap()
}

// ---- registry --------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn add_registers_the_canonical_root_with_detected_provider() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("my-project");
    repo_with_commit(&root, Some("https://github.com/owner/my-project.git"));

    let repo = t.engine.add_repository(root.join("."), None).await.unwrap();
    assert_eq!(repo.name, "my-project");
    assert_eq!(repo.path, canonical(&root));
    assert_eq!(repo.provider, Some(Provider::Github));
    assert_eq!(t.sink.take(), vec![AppEvent::RepositoriesUpdated(vec![repo.clone()])]);
    assert_eq!(t.engine.repositories().await.unwrap(), vec![repo]);
}

#[tokio::test(flavor = "multi_thread")]
async fn add_with_explicit_provider_overrides_detection() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    repo_with_commit(dir.path(), Some("https://git.example.com/x/y.git"));
    let repo = t.engine.add_repository(dir.path().into(), Some(Provider::Gitlab)).await.unwrap();
    assert_eq!(repo.provider, Some(Provider::Gitlab));

    let dir2 = tempfile::tempdir().unwrap();
    let r = repo_with_commit(dir2.path(), Some("https://git.example.com/x/y.git"));
    r.remote("mirror", "https://dev.azure.com/org/p/_git/y").unwrap();
    // `origin` has no known host: the first remote that has one wins.
    assert_eq!(add(&t, dir2.path()).await.provider, Some(Provider::AzureDevops));
}

#[tokio::test(flavor = "multi_thread")]
async fn add_from_subfolder_or_git_dir_registers_the_root_and_duplicates_are_rejected() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("proj");
    repo_with_commit(&root, None);
    let sub = root.join("src").join("deep");
    std::fs::create_dir_all(&sub).unwrap();

    let repo = add(&t, &sub).await;
    assert_eq!(repo.path, canonical(&root));
    assert_eq!(repo.name, "proj");
    t.sink.take();

    for again in [root.clone(), root.join(".git"), root.join("src/../"), sub] {
        let err = t.engine.add_repository(again.clone(), None).await;
        assert!(matches!(err, Err(GitYasError::AlreadyExists(_))), "{again:?}: {err:?}");
    }
    #[cfg(unix)]
    {
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert!(matches!(
            t.engine.add_repository(link, None).await,
            Err(GitYasError::AlreadyExists(_))
        ));
    }
    assert!(t.sink.take().is_empty(), "no event for a rejected add");
    assert_eq!(t.engine.repositories().await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn add_bare_and_linked_worktree() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let bare = bare_remote(dir.path(), 1);
    let repo = add(&t, &bare).await;
    assert_eq!(repo.path, canonical(&bare));
    assert_eq!(repo.name, "remote");

    let main = git2::Repository::open(dir.path().join("work")).unwrap();
    let wt_path = dir.path().join("feature-wt");
    main.worktree("feature-wt", &wt_path, None).unwrap();
    let wt = add(&t, &wt_path).await;
    assert_eq!(wt.path, canonical(&wt_path));
    assert_eq!(wt.name, "feature-wt");
    // The main checkout is a different repository root.
    assert_eq!(add(&t, &dir.path().join("work")).await.name, "work");
}

#[tokio::test(flavor = "multi_thread")]
async fn add_rejects_non_repositories_and_bad_paths() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    assert!(matches!(t.engine.add_repository(plain, None).await, Err(GitYasError::NotFound(_))));
    let missing = dir.path().join("missing");
    assert!(matches!(
        t.engine.add_repository(missing, None).await,
        Err(GitYasError::NotFound(_))
    ));
    assert!(matches!(
        t.engine.add_repository(PathBuf::new(), None).await,
        Err(GitYasError::InvalidInput(_))
    ));
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let weird = dir.path().join(std::ffi::OsStr::from_bytes(b"caf\xe9"));
        // Creating a non-UTF-8 name can fail on file systems that enforce UTF-8 (APFS); the path is rejected first anyway.
        let _created = std::fs::create_dir(&weird).is_ok_and(|()| git2::Repository::init(&weird).is_ok());
        assert!(matches!(
            t.engine.add_repository(weird, None).await,
            Err(GitYasError::InvalidInput(_))
        ));
    }
    assert!(t.sink.take().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn reorder_and_set_provider() {
    let t = test_engine();
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut ids = Vec::new();
    for d in &dirs {
        repo_with_commit(d.path(), None);
        ids.push(add(&t, d.path()).await.id);
    }
    t.sink.take();
    let reversed: Vec<_> = ids.iter().rev().cloned().collect();
    t.engine.reorder_repositories(reversed.clone()).await.unwrap();
    let listed: Vec<_> = t.engine.repositories().await.unwrap().into_iter().map(|r| r.id).collect();
    assert_eq!(listed, reversed);
    match t.sink.take().as_slice() {
        [AppEvent::RepositoriesUpdated(repos)] => {
            assert_eq!(repos.iter().map(|r| &r.id).collect::<Vec<_>>(), reversed.iter().collect::<Vec<_>>())
        }
        other => panic!("{other:?}"),
    }
    let partial = vec![ids[0].clone()];
    assert!(matches!(
        t.engine.reorder_repositories(partial).await,
        Err(GitYasError::InvalidInput(_))
    ));

    // Provider change reaches the active session's context.
    t.engine.open_repository(ids[1].clone()).await.unwrap();
    t.engine
        .set_repository_provider(ids[1].clone(), Some(Provider::Bitbucket))
        .await
        .unwrap();
    assert_eq!(t.engine.inner.sessions.require().unwrap().provider, Some(Provider::Bitbucket));
    let repos = t.engine.repositories().await.unwrap();
    assert_eq!(repos.iter().find(|r| r.id == ids[1]).unwrap().provider, Some(Provider::Bitbucket));
    assert_eq!(names(&t.sink.take()).last(), Some(&"RepositoriesUpdated"));
    t.engine
        .set_repository_provider(ids[0].clone(), Some(Provider::Github))
        .await
        .unwrap();
    assert_eq!(t.engine.inner.sessions.require().unwrap().provider, Some(Provider::Bitbucket));
    assert!(matches!(
        t.engine.set_repository_provider("nope".into(), None).await,
        Err(GitYasError::NotFound(_))
    ));
    t.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_active_repository_closes_the_session_and_keeps_files() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    repo_with_commit(dir.path(), None);
    let repo = add(&t, dir.path()).await;
    t.engine.open_repository(repo.id.clone()).await.unwrap();
    let ctx = t.engine.inner.sessions.require().unwrap();
    t.sink.take();

    t.engine.delete_repository(repo.id.clone()).await.unwrap();
    assert!(ctx.is_cancelled());
    assert_eq!(t.engine.active_repository(), None);
    assert_eq!(
        t.sink.take(),
        vec![AppEvent::SelectedRepositoryUpdated(None), AppEvent::RepositoriesUpdated(vec![])]
    );
    assert_eq!(t.engine.open_tabs().await.unwrap(), OpenTabs::default());
    assert!(dir.path().join("README.md").exists() && dir.path().join(".git").exists());
    assert!(matches!(t.engine.delete_repository(repo.id).await, Err(GitYasError::NotFound(_))));
    assert!(t.sink.take().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_inactive_repository_keeps_the_session() {
    let t = test_engine();
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    repo_with_commit(a.path(), None);
    repo_with_commit(b.path(), None);
    let ra = add(&t, a.path()).await;
    let rb = add(&t, b.path()).await;
    t.engine.open_repository(ra.id.clone()).await.unwrap();
    let ctx = t.engine.inner.sessions.require().unwrap();
    t.sink.take();
    t.engine.delete_repository(rb.id).await.unwrap();
    assert!(!ctx.is_cancelled());
    assert_eq!(t.engine.active_repository(), Some(ra.id.clone()));
    assert_eq!(t.sink.take(), vec![AppEvent::RepositoriesUpdated(vec![ra])]);
    t.engine.shutdown().await.unwrap();
}

/// Needs WP8e's `cache::purge_repository` (a no-op in the Wave 2 skeleton). Run after integration.
#[tokio::test(flavor = "multi_thread")]
async fn delete_repository_purges_cached_history() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    repo_with_commit(dir.path(), None);
    let repo = add(&t, dir.path()).await;
    t.engine.open_repository(repo.id.clone()).await.unwrap();
    assert!(
        t.engine.inner.storage.cache().snapshot(&repo.id).unwrap().is_some(),
        "open populates the cache"
    );
    t.engine.delete_repository(repo.id.clone()).await.unwrap();
    assert!(t.engine.inner.storage.cache().snapshot(&repo.id).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn detect_provider_matches_hosted_urls_only() {
    let t = test_engine();
    assert_eq!(t.engine.detect_provider("https://github.com/o/r.git"), Some(Provider::Github));
    assert_eq!(t.engine.detect_provider("git@gitlab.com:o/r.git"), Some(Provider::Gitlab));
    assert_eq!(
        t.engine.detect_provider("https://dev.azure.com/o/p/_git/r"),
        Some(Provider::AzureDevops)
    );
    assert_eq!(t.engine.detect_provider("https://git.example.com/o/r.git"), None);
    assert_eq!(t.engine.detect_provider("/srv/git/r.git"), None);
}

// ---- clone -----------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn clone_from_a_local_bare_repository_registers_it() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let bare = bare_remote(dir.path(), 3);
    let url = file_url(&bare);
    let dest = t.engine.clone_destination(&url, &dir.path().join("into")).unwrap();
    assert_eq!(dest, dir.path().join("into").join("remote"));

    let repo = t.engine.clone_repository(url, dest.clone(), None).await.unwrap();
    assert_eq!(repo.path, canonical(&dest));
    assert_eq!(repo.name, "remote");
    assert_eq!(repo.provider, None);
    assert!(dest.join("file2.txt").exists());
    let events = t.sink.take();
    assert_eq!(events.last(), Some(&AppEvent::RepositoriesUpdated(vec![repo.clone()])));
    for event in &events[..events.len() - 1] {
        match event {
            AppEvent::RemoteProgress { repo: None, progress } => assert_eq!(progress.op, RemoteOp::Clone),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(t.engine.active_repository(), None, "clone does not open");
    assert_eq!(t.engine.cancel_remote_operation(), 0, "remote op unregistered");

    // Into an existing non-empty folder: refused, folder untouched.
    let err = t.engine.clone_repository(bare.display().to_string(), dest.clone(), None).await;
    assert!(matches!(err, Err(GitYasError::InvalidInput(_))), "{err:?}");
    assert!(dest.join("file2.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_clone_removes_only_what_it_created() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("does-not-exist.git");
    let dest = dir.path().join("a").join("b");
    let err = t.engine.clone_repository(missing.display().to_string(), dest, None).await;
    assert!(err.is_err());
    assert!(!dir.path().join("a").exists(), "created parents removed");

    let empty = dir.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let err = t.engine.clone_repository(missing.display().to_string(), empty.clone(), None).await;
    assert!(err.is_err());
    assert!(empty.is_dir(), "pre-existing folder kept");
    assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0);
    assert!(t.engine.repositories().await.unwrap().is_empty());

    let rel = t
        .engine
        .clone_repository("https://github.com/o/r".into(), PathBuf::from("rel/r"), None)
        .await;
    assert!(matches!(rel, Err(GitYasError::InvalidInput(_))));
    let blank = t.engine.clone_repository("  ".into(), dir.path().join("x"), None).await;
    assert!(matches!(blank, Err(GitYasError::InvalidInput(_))));
}

/// Cancels every remote operation as soon as the first clone progress report arrives.
struct CancelOnProgress {
    engine: Mutex<Option<Engine>>,
    events: RecordingSink,
}

impl EventSink for CancelOnProgress {
    fn emit(&self, event: AppEvent) {
        if matches!(event, AppEvent::RemoteProgress { .. })
            && let Some(engine) = self.engine.lock().unwrap().as_ref()
        {
            engine.cancel_remote_operation();
        }
        self.events.emit(event);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_clone_fails_with_cancelled_and_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let bare = bare_remote(dir.path(), 20);
    let sink = Arc::new(CancelOnProgress {
        engine: Mutex::new(None),
        events: RecordingSink::new(),
    });
    let engine = Engine::with_options(
        Storage::in_memory().unwrap(),
        Arc::new(MemoryCredentialStore::new()),
        sink.clone(),
        tokio::runtime::Handle::current(),
        options(),
    );
    *sink.engine.lock().unwrap() = Some(engine.clone());

    for dest in [dir.path().join("fresh"), dir.path().join("empty")] {
        let pre_existing = dest.ends_with("empty");
        if pre_existing {
            std::fs::create_dir(&dest).unwrap();
        }
        let url = file_url(&bare);
        let result = engine.clone_repository(url, dest.clone(), None).await;
        assert_eq!(result.err(), Some(GitYasError::Cancelled), "{dest:?}");
        if pre_existing {
            assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 0);
        } else {
            assert!(!dest.exists());
        }
    }
    let events = sink.events.take();
    assert!(names(&events).iter().all(|n| *n == "RemoteProgress"), "{events:?}");
    assert!(engine.repositories().await.unwrap().is_empty());
    *sink.engine.lock().unwrap() = None;
}

// ---- lifecycle -------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn open_emits_selection_then_full_state_and_records_the_tab() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    // Empty repository with an unborn HEAD.
    git2::Repository::init(dir.path()).unwrap();
    let repo = add(&t, dir.path()).await;
    t.sink.take();

    t.engine.open_repository(repo.id.clone()).await.unwrap();
    let events = t.sink.take();
    assert_eq!(
        names(&events),
        [
            "SelectedRepositoryUpdated",
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
    assert_eq!(events[0], AppEvent::SelectedRepositoryUpdated(Some(repo.id.clone())));
    assert!(events[1..].iter().all(|e| e.repo() == Some(&repo.id)));
    assert_eq!(t.engine.active_repository(), Some(repo.id.clone()));
    assert_eq!(
        t.engine.open_tabs().await.unwrap(),
        OpenTabs {
            open: vec![repo.id.clone()],
            active: Some(repo.id.clone())
        }
    );

    // Re-opening the active repository keeps the session and re-emits the state.
    let ctx = t.engine.inner.sessions.require().unwrap();
    t.engine.open_repository(repo.id.clone()).await.unwrap();
    let again = t.engine.inner.sessions.require().unwrap();
    assert_eq!(again.generation, ctx.generation);
    assert!(!ctx.is_cancelled());
    assert_eq!(names(&t.sink.take()).len(), 9);

    t.engine.close_repository().await.unwrap();
    assert!(ctx.is_cancelled());
    assert_eq!(t.engine.active_repository(), None);
    assert_eq!(t.sink.take(), vec![AppEvent::SelectedRepositoryUpdated(None)]);
    let tabs = t.engine.open_tabs().await.unwrap();
    assert_eq!(tabs.open, vec![repo.id.clone()]);
    assert_eq!(tabs.active, None);
    // Idempotent; commands now fail cleanly.
    t.engine.close_repository().await.unwrap();
    assert!(t.sink.take().is_empty());
    assert_eq!(t.engine.refresh(crate::Refresh::ALL).await, Err(GitYasError::NoActiveRepository));
}

#[tokio::test(flavor = "multi_thread")]
async fn open_bare_repository_still_succeeds() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    let bare = bare_remote(dir.path(), 1);
    let repo = add(&t, &bare).await;
    t.sink.take();
    t.engine.open_repository(repo.id.clone()).await.unwrap();
    let events = t.sink.take();
    assert_eq!(events.first(), Some(&AppEvent::SelectedRepositoryUpdated(Some(repo.id.clone()))));
    assert!(names(&events).contains(&"CommitsUpdated"));
    t.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn switching_cancels_the_old_session_and_drops_its_events() {
    let t = test_engine();
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    repo_with_commit(a.path(), None);
    repo_with_commit(b.path(), None);
    let ra = add(&t, a.path()).await;
    let rb = add(&t, b.path()).await;

    t.engine.open_repository(ra.id.clone()).await.unwrap();
    let old = t.engine.inner.sessions.require().unwrap();
    let in_flight = t.engine.inner.sessions.begin_remote_op(Some(&old));
    t.sink.take();

    t.engine.open_repository(rb.id.clone()).await.unwrap();
    assert!(old.is_cancelled());
    assert!(in_flight.flag().load(std::sync::atomic::Ordering::SeqCst), "old remote op aborted");
    let new = t.engine.inner.sessions.require().unwrap();
    assert!(new.generation > old.generation);
    assert_eq!(new.repo_id, rb.id);
    let events = t.sink.take();
    assert_eq!(events[0], AppEvent::SelectedRepositoryUpdated(Some(rb.id.clone())));
    assert!(events[1..].iter().all(|e| e.repo() == Some(&rb.id)));

    // A late event computed for the old session never reaches the sink.
    t.engine.inner.emit_scoped(
        &old,
        AppEvent::RemotesUpdated {
            repo: ra.id.clone(),
            remotes: vec![],
        },
    );
    assert!(t.sink.take().is_empty());
    let tabs = t.engine.open_tabs().await.unwrap();
    assert_eq!(tabs.open, vec![ra.id.clone(), rb.id.clone()]);
    assert_eq!(tabs.active, Some(rb.id));
    drop(in_flight);
    t.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn open_unknown_or_vanished_repository_leaves_the_session_alone() {
    let t = test_engine();
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    repo_with_commit(a.path(), None);
    repo_with_commit(b.path(), None);
    let ra = add(&t, a.path()).await;
    let rb = add(&t, b.path()).await;
    t.engine.open_repository(ra.id.clone()).await.unwrap();
    let ctx = t.engine.inner.sessions.require().unwrap();
    t.sink.take();

    assert!(matches!(
        t.engine.open_repository("unknown".into()).await,
        Err(GitYasError::NotFound(_))
    ));
    let b_path = b.path().to_path_buf();
    drop(b);
    assert!(!b_path.exists());
    assert!(matches!(t.engine.open_repository(rb.id).await, Err(GitYasError::NotFound(_))));
    assert!(!ctx.is_cancelled());
    assert_eq!(t.engine.active_repository(), Some(ra.id));
    assert!(t.sink.take().is_empty());
    t.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_is_idempotent_and_keeps_tabs() {
    let t = test_engine();
    let dir = tempfile::tempdir().unwrap();
    repo_with_commit(dir.path(), None);
    let repo = add(&t, dir.path()).await;
    t.engine.open_repository(repo.id.clone()).await.unwrap();
    let ctx = t.engine.inner.sessions.require().unwrap();
    let clone_op = t.engine.inner.sessions.begin_remote_op(None);
    t.engine.shutdown().await.unwrap();
    t.engine.shutdown().await.unwrap();
    assert!(ctx.is_cancelled());
    assert!(clone_op.flag().load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(t.engine.active_repository(), None);
    assert_eq!(t.engine.open_tabs().await.unwrap().active, Some(repo.id.clone()));
    // Terminal for sessions (WP8f.1).
    assert_eq!(t.engine.open_repository(repo.id).await.err(), Some(GitYasError::Cancelled));
    assert_eq!(t.engine.active_repository(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn set_open_tabs_is_normalised() {
    let t = test_engine();
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    repo_with_commit(a.path(), None);
    repo_with_commit(b.path(), None);
    let ra = add(&t, a.path()).await;
    let rb = add(&t, b.path()).await;
    let tabs = OpenTabs {
        open: vec![rb.id.clone(), "gone".into(), ra.id.clone(), rb.id.clone()],
        active: Some("gone".into()),
    };
    t.engine.set_open_tabs(tabs).await.unwrap();
    assert_eq!(
        t.engine.open_tabs().await.unwrap(),
        OpenTabs {
            open: vec![rb.id.clone(), ra.id.clone()],
            active: Some(rb.id.clone())
        }
    );
    t.engine
        .set_open_tabs(OpenTabs {
            open: vec![ra.id.clone()],
            active: None,
        })
        .await
        .unwrap();
    assert_eq!(t.engine.open_tabs().await.unwrap().active, None);
    assert_eq!(t.engine.active_repository(), None, "set_open_tabs does not switch");
}

#[tokio::test(flavor = "multi_thread")]
async fn start_restores_the_last_active_repository_after_restart() {
    let data = tempfile::tempdir().unwrap();
    let repos = tempfile::tempdir().unwrap();
    let (a, b) = (repos.path().join("a"), repos.path().join("b"));
    repo_with_commit(&a, None);
    repo_with_commit(&b, Some("https://gitlab.com/o/b.git"));
    let credentials = Arc::new(MemoryCredentialStore::new());
    credentials
        .set(Provider::Gitlab, Credential::new(None, "glpat-abcdefghijklmnopqrst"))
        .unwrap();

    let (ra, rb) = {
        let (engine, sink) = engine_on(disk_storage(data.path()), credentials.clone());
        engine.start().await.unwrap();
        assert_eq!(
            names(&sink.take()),
            [
                "RepositoriesUpdated",
                "AuthStatusUpdated",
                "AuthStatusUpdated",
                "AuthStatusUpdated",
                "AuthStatusUpdated",
                "SelectedRepositoryUpdated"
            ]
        );
        let ra = engine.add_repository(a.clone(), None).await.unwrap();
        let rb = engine.add_repository(b.clone(), None).await.unwrap();
        engine
            .update_commit_signature(gityas_core::models::CommitSignature::new("Ada", "ada@example.com"))
            .await
            .unwrap();
        engine.open_repository(ra.id.clone()).await.unwrap();
        engine.open_repository(rb.id.clone()).await.unwrap();
        engine.shutdown().await.unwrap();
        (ra, rb)
    };

    let (engine, sink) = engine_on(disk_storage(data.path()), credentials);
    engine.start().await.unwrap();
    assert_eq!(engine.active_repository(), Some(rb.id.clone()));
    let events = sink.take();
    assert_eq!(events[0], AppEvent::RepositoriesUpdated(vec![ra.clone(), rb.clone()]));
    assert_eq!(
        events[1],
        AppEvent::CommitsSignatureUpdated(gityas_core::models::CommitSignature::new("Ada", "ada@example.com"))
    );
    let auth: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AppEvent::AuthStatusUpdated { provider, connected } => Some((*provider, *connected)),
            _ => None,
        })
        .collect();
    assert_eq!(auth.len(), Provider::ALL.len());
    assert!(auth.contains(&(Provider::Gitlab, true)) && auth.contains(&(Provider::Github, false)));
    assert_eq!(events[6], AppEvent::SelectedRepositoryUpdated(Some(rb.id.clone())));
    assert!(names(&events).contains(&"CommitsUpdated"));
    assert!(!names(&events).contains(&"Warning"));
    assert_eq!(engine.open_tabs().await.unwrap().open, vec![ra.id, rb.id]);
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn start_with_a_deleted_repository_warns_and_falls_back() {
    let data = tempfile::tempdir().unwrap();
    let repos = tempfile::tempdir().unwrap();
    let (a, b) = (repos.path().join("a"), repos.path().join("b"));
    repo_with_commit(&a, None);
    repo_with_commit(&b, None);
    let credentials = Arc::new(MemoryCredentialStore::new());
    let (ra, rb) = {
        let (engine, _sink) = engine_on(disk_storage(data.path()), credentials.clone());
        let ra = engine.add_repository(a.clone(), None).await.unwrap();
        let rb = engine.add_repository(b.clone(), None).await.unwrap();
        engine.open_repository(ra.id.clone()).await.unwrap();
        engine.open_repository(rb.id.clone()).await.unwrap();
        engine.shutdown().await.unwrap();
        (ra, rb)
    };
    std::fs::remove_dir_all(&b).unwrap();

    let (engine, sink) = engine_on(disk_storage(data.path()), credentials.clone());
    engine.start().await.unwrap();
    let events = sink.take();
    let warnings: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AppEvent::Warning(w) => Some(w.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(warnings.len(), 1, "{events:?}");
    assert!(warnings[0].starts_with("Could not reopen b:"), "{}", warnings[0]);
    // The neighbouring tab was opened instead; the missing repo stays registered but its tab is gone.
    assert_eq!(engine.active_repository(), Some(ra.id.clone()));
    assert_eq!(
        engine.open_tabs().await.unwrap(),
        OpenTabs {
            open: vec![ra.id.clone()],
            active: Some(ra.id.clone())
        }
    );
    assert_eq!(engine.repositories().await.unwrap().len(), 2);
    engine.shutdown().await.unwrap();

    // With no openable tab left: warning, no selection, start still succeeds.
    std::fs::remove_dir_all(&a).unwrap();
    let (engine, sink) = engine_on(disk_storage(data.path()), credentials);
    engine.start().await.unwrap();
    let events = sink.take();
    assert_eq!(names(&events).iter().filter(|n| **n == "Warning").count(), 1);
    assert_eq!(events.last(), Some(&AppEvent::SelectedRepositoryUpdated(None)));
    assert_eq!(engine.active_repository(), None);
    assert_eq!(engine.open_tabs().await.unwrap(), OpenTabs::default());
    drop(rb);
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_warnings_are_reported() {
    let t = test_engine();
    t.engine.report_storage_warnings(vec!["db was reset".into()]);
    assert_eq!(t.sink.take(), vec![AppEvent::Warning("db was reset".into())]);
}
