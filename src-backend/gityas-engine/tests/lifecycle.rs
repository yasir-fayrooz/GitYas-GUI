//! WP8f end-to-end scenarios through the public `Engine` API: commit flow, restart, delete, credentials, shutdown.

// Test helpers outside `#[test]` fns: `allow-unwrap-in-tests` does not reach them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::*;
use gityas_auth::CredentialStore;
use gityas_core::models::{BranchKind, CommitSignature, DeltaChange, OpenTabs};
use gityas_core::{AppEvent, GitYasError, Provider};
use gityas_engine::{EngineOptions, Refresh};
use secrecy::SecretString;

const OPEN_STATE: [&str; 7] = [
    "SelectedRepositoryUpdated",
    "CommitsUpdated",
    "CommitsHeadUpdated",
    "LocalBranchesUpdated",
    "RemoteBranchesUpdated",
    "RemotesUpdated",
    "GitFilesChanged",
];

fn last_files(events: &[AppEvent]) -> gityas_core::models::FileState {
    events
        .iter()
        .rev()
        .find_map(|ev| match ev {
            AppEvent::GitFilesChanged { files, .. } => Some(files.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no GitFilesChanged in {:?}", names(events)))
}

/// Scenario 1: add → open → modify → refresh → stage → commit, asserting the event stream at each step.
#[tokio::test(flavor = "multi_thread")]
async fn add_open_modify_stage_commit_emits_the_expected_events() {
    let h = engine_in_memory(options());
    let dir = tempfile::tempdir().unwrap();
    let root = init_repo(dir.path());
    let other = tempfile::tempdir().unwrap();
    init_repo(other.path());

    step(
        "signature",
        h.engine.update_commit_signature(CommitSignature::new("Ada", "ada@example.com")),
    )
    .await
    .unwrap();
    let repo = step("add", h.engine.add_repository(root.clone(), None)).await.unwrap();
    let other_repo = step("add other", h.engine.add_repository(other.path().to_path_buf(), None))
        .await
        .unwrap();
    assert_eq!(repo.path, root);
    let events = h.sink.take();
    assert_eq!(
        names(&events),
        ["CommitsSignatureUpdated", "RepositoriesUpdated", "RepositoriesUpdated"]
    );
    match &events[2] {
        AppEvent::RepositoriesUpdated(repos) => {
            assert_eq!(repos.iter().map(|r| &r.id).collect::<Vec<_>>(), [&repo.id, &other_repo.id]);
        }
        other => panic!("{other:?}"),
    }

    // Open: the selection comes first, then the full state, exactly once each.
    step("open", h.engine.open_repository(repo.id.clone())).await.unwrap();
    assert_eq!(h.engine.active_repository(), Some(repo.id.clone()));
    let events = h.sink.take();
    assert!(events.len() >= OPEN_STATE.len(), "{:?}", names(&events));
    assert_eq!(names(&events[..OPEN_STATE.len()]), OPEN_STATE);
    assert_eq!(events[0], AppEvent::SelectedRepositoryUpdated(Some(repo.id.clone())));
    assert_only_repo(&events, &repo.id);
    let initial = last_commits(&events, &repo.id).unwrap();
    assert_eq!(initial.commits.len(), 1);

    // Modify + explicit refresh (the native watcher does not deliver in the sandbox; anything it adds is tolerated).
    std::fs::write(root.join("README.md"), "hello\nworld\n").unwrap();
    step("refresh", h.engine.refresh(Refresh::FILES)).await.unwrap();
    let events = h.sink.take();
    assert_only_repo(&events, &repo.id);
    let files = last_files(&events);
    assert!(files.staged.is_empty());
    assert_eq!(files.unstaged.len(), 1, "{files:?}");
    assert_eq!(files.unstaged[0].path, Path::new("README.md"));
    assert_eq!(files.unstaged[0].change, DeltaChange::Modified);

    // Stage.
    let state = step("file_state", h.engine.file_state()).await.unwrap();
    assert_eq!(state, files);
    step("stage", h.engine.stage_files(state.unstaged.clone())).await.unwrap();
    let events = h.sink.take();
    assert_only_repo(&events, &repo.id);
    assert!(names(&events).contains(&"GitFilesChanged"));
    let files = last_files(&events);
    assert!(files.unstaged.is_empty(), "{files:?}");
    assert_eq!(files.staged.len(), 1);
    assert_eq!(files.staged[0].path, Path::new("README.md"));

    // Commit.
    let oid = step("commit", h.engine.commit_staged_files("Second".into(), Some("body".into())))
        .await
        .unwrap();
    let git_head = git2::Repository::open(&root).unwrap().head().unwrap().target().unwrap();
    assert_eq!(oid, to_core(git_head), "returned Oid is HEAD");
    let events = h.sink.take();
    assert_only_repo(&events, &repo.id);
    assert_subsequence(
        &events,
        &["CommitsUpdated", "CommitsHeadUpdated", "LocalBranchesUpdated", "GitFilesChanged"],
    );
    let commits = last_commits(&events, &repo.id).unwrap();
    assert_eq!(commits.commits.len(), 2);
    assert_eq!(commits.commits[0].id, oid, "new commit first");
    assert_eq!(commits.commits[0].summary, "Second");
    assert_eq!(commits.commits[0].author_name, "Ada");
    assert_eq!(commits.commits[0].parents, vec![initial.commits[0].id.clone()]);
    let head = events
        .iter()
        .find_map(|ev| match ev {
            AppEvent::CommitsHeadUpdated { head, .. } => head.clone(),
            _ => None,
        })
        .unwrap();
    assert_eq!(head.target(), Some(&oid));
    let branches = events
        .iter()
        .find_map(|ev| match ev {
            AppEvent::LocalBranchesUpdated { branches, .. } => Some(branches.clone()),
            _ => None,
        })
        .unwrap();
    let main = branches.iter().find(|b| b.refname == "refs/heads/main").unwrap();
    assert_eq!(main.target.as_ref(), Some(&oid));
    assert_eq!(last_files(&events), Default::default());

    // Query APIs agree with the events.
    assert_eq!(step("head", h.engine.head()).await.unwrap(), Some(head));
    assert_eq!(step("branches", h.engine.branches(BranchKind::Local)).await.unwrap(), branches);
    step("shutdown", h.engine.shutdown()).await.unwrap();
}

/// Scenario 3: state survives a restart on the same SQLite files.
#[tokio::test(flavor = "multi_thread")]
async fn restart_restores_registry_order_signature_tabs_and_the_active_repository() {
    let data = tempfile::tempdir().unwrap();
    let repos = tempfile::tempdir().unwrap();
    let a_path = init_repo(&repos.path().join("a"));
    let b_path = init_repo(&repos.path().join("b"));
    commit_file(&b_path, "b.txt", "b\n", "b only");
    let signature = CommitSignature::new("Ada", "ada@example.com");

    let (a, b) = {
        let h = engine_on_disk(data.path(), options());
        step("start empty", h.engine.start()).await.unwrap();
        let a = h.engine.add_repository(a_path.clone(), None).await.unwrap();
        let b = h.engine.add_repository(b_path.clone(), None).await.unwrap();
        h.engine.reorder_repositories(vec![b.id.clone(), a.id.clone()]).await.unwrap();
        h.engine
            .set_open_tabs(OpenTabs {
                open: vec![a.id.clone(), b.id.clone()],
                active: Some(a.id.clone()),
            })
            .await
            .unwrap();
        h.engine.update_commit_signature(signature.clone()).await.unwrap();
        step("open a", h.engine.open_repository(a.id.clone())).await.unwrap();
        step("open b", h.engine.open_repository(b.id.clone())).await.unwrap();
        step("shutdown", h.engine.shutdown()).await.unwrap();
        (a, b)
    };

    let h = engine_on_disk(data.path(), options());
    step("start", h.engine.start()).await.unwrap();
    let events = h.sink.take();
    let n = names(&events);
    assert_eq!(n[0], "RepositoriesUpdated", "{n:?}");
    match &events[0] {
        AppEvent::RepositoriesUpdated(list) => {
            assert_eq!(list.iter().map(|r| r.id.clone()).collect::<Vec<_>>(), [b.id.clone(), a.id.clone()]);
            assert_eq!(list[0].path, b_path);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(events[1], AppEvent::CommitsSignatureUpdated(signature));
    let auth: Vec<_> = events[2..2 + Provider::ALL.len()].to_vec();
    assert_eq!(
        auth,
        Provider::ALL
            .iter()
            .map(|&provider| AppEvent::AuthStatusUpdated {
                provider,
                connected: false
            })
            .collect::<Vec<_>>()
    );
    let rest = &events[2 + Provider::ALL.len()..];
    assert_eq!(names(&rest[..OPEN_STATE.len()]), OPEN_STATE);
    assert_eq!(rest[0], AppEvent::SelectedRepositoryUpdated(Some(b.id.clone())));
    assert_only_repo(rest, &b.id);
    assert_eq!(last_commits(rest, &b.id).unwrap().commits[0].summary, "b only");
    assert_eq!(h.engine.active_repository(), Some(b.id.clone()));
    assert_eq!(
        h.engine.open_tabs().await.unwrap(),
        OpenTabs {
            open: vec![a.id.clone(), b.id.clone()],
            active: Some(b.id.clone()),
        }
    );
    h.engine.shutdown().await.unwrap();
}

/// Scenario 5: deleting the active repository closes the session, and a restart does not reopen it.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_the_active_repository_closes_the_session_for_good() {
    let data = tempfile::tempdir().unwrap();
    let repos = tempfile::tempdir().unwrap();
    let a_path = init_repo(&repos.path().join("a"));
    let b_path = init_repo(&repos.path().join("b"));

    let (a, b) = {
        let h = engine_on_disk(data.path(), options());
        let a = h.engine.add_repository(a_path.clone(), None).await.unwrap();
        let b = h.engine.add_repository(b_path, None).await.unwrap();
        step("open b", h.engine.open_repository(b.id.clone())).await.unwrap();
        step("open a", h.engine.open_repository(a.id.clone())).await.unwrap();
        h.sink.take();

        step("delete", h.engine.delete_repository(a.id.clone())).await.unwrap();
        let events = h.sink.take();
        assert_eq!(names(&events), ["SelectedRepositoryUpdated", "RepositoriesUpdated"]);
        assert_eq!(events[0], AppEvent::SelectedRepositoryUpdated(None));
        match &events[1] {
            AppEvent::RepositoriesUpdated(list) => assert_eq!(list.iter().map(|r| &r.id).collect::<Vec<_>>(), [&b.id]),
            other => panic!("{other:?}"),
        }
        assert_eq!(h.engine.active_repository(), None);
        assert_eq!(h.engine.file_state().await.err(), Some(GitYasError::NoActiveRepository));
        assert_eq!(h.engine.refresh(Refresh::ALL).await.err(), Some(GitYasError::NoActiveRepository));
        assert_eq!(h.engine.fetch(None).await.err(), Some(GitYasError::NoActiveRepository));
        assert!(matches!(
            h.engine.open_repository(a.id.clone()).await,
            Err(GitYasError::NotFound(_))
        ));
        assert!(
            h.sink.take().iter().all(|ev| ev.repo().is_none()),
            "no repo-scoped events after delete"
        );
        // The files are never deleted.
        assert!(a_path.join(".git").exists());
        let tabs = h.engine.open_tabs().await.unwrap();
        assert!(!tabs.is_open(&a.id), "{tabs:?}");
        h.engine.shutdown().await.unwrap();
        (a, b)
    };
    assert_eq!(cached_history_len(data.path(), &a.id), None, "cache rows purged");
    assert_eq!(cached_history_len(data.path(), &b.id), Some(1));

    let h = engine_on_disk(data.path(), options());
    step("start", h.engine.start()).await.unwrap();
    let events = h.sink.take();
    assert!(events.iter().all(|ev| ev.repo() != Some(&a.id)), "{:?}", names(&events));
    assert!(!events.contains(&AppEvent::SelectedRepositoryUpdated(Some(a.id.clone()))));
    assert_ne!(h.engine.active_repository(), Some(a.id.clone()));
    let repos = h.engine.repositories().await.unwrap();
    assert_eq!(repos.iter().map(|r| &r.id).collect::<Vec<_>>(), [&b.id]);
    h.engine.shutdown().await.unwrap();
}

/// Scenario 6: credentials go through the engine with events; `test_connection` needs a session.
#[tokio::test(flavor = "multi_thread")]
async fn credentials_round_trip_with_events_and_test_connection_needs_a_session() {
    let h = engine_in_memory(options());
    assert_eq!(
        h.engine.test_connection(Provider::Github).await.err(),
        Some(GitYasError::NoActiveRepository)
    );

    step(
        "set",
        h.engine
            .set_credential(Provider::Gitlab, Some(" oauth2 ".into()), SecretString::from(" glpat-123 ")),
    )
    .await
    .unwrap();
    assert_eq!(
        h.sink.take(),
        [AppEvent::AuthStatusUpdated {
            provider: Provider::Gitlab,
            connected: true
        }]
    );
    let status = h.engine.auth_status().await.unwrap();
    assert!(status.contains(&(Provider::Gitlab, true)) && status.contains(&(Provider::Github, false)));
    assert_eq!(h.credentials.status(), status, "the engine's store is the one it reports");

    // Invalid token: rejected, nothing emitted, previous credential kept.
    assert!(matches!(
        h.engine.set_credential(Provider::Gitlab, None, SecretString::from("a\nb")).await,
        Err(GitYasError::InvalidInput(_) | GitYasError::Auth(_))
    ));
    assert!(h.sink.take().is_empty());
    assert!(h.engine.auth_status().await.unwrap().contains(&(Provider::Gitlab, true)));

    step("delete", h.engine.delete_credential(Provider::Gitlab)).await.unwrap();
    step("delete again", h.engine.delete_credential(Provider::Gitlab)).await.unwrap();
    let off = AppEvent::AuthStatusUpdated {
        provider: Provider::Gitlab,
        connected: false,
    };
    assert_eq!(h.sink.take(), [off.clone(), off]);
    assert!(h.engine.auth_status().await.unwrap().iter().all(|(_, c)| !c));

    // With a session but no remote for the provider: a clear error, no repo-scoped events, session untouched.
    let dir = tempfile::tempdir().unwrap();
    let root = init_repo(dir.path());
    let repo = h.engine.add_repository(root, None).await.unwrap();
    h.engine.open_repository(repo.id.clone()).await.unwrap();
    h.sink.take();
    let err = step("test_connection", h.engine.test_connection(Provider::Github))
        .await
        .unwrap_err();
    assert!(matches!(err, GitYasError::NotFound(_) | GitYasError::Auth(_)), "{err:?}");
    assert!(h.sink.take().iter().all(|ev| ev.repo().is_none()));
    assert_eq!(h.engine.active_repository(), Some(repo.id));
    h.engine.shutdown().await.unwrap();
}

/// Scenario 7: shutdown is idempotent and terminal for sessions. Afterwards repo commands fail with
/// `NoActiveRepository`, `open_repository` / `start` fail with `Cancelled` (no watcher or scheduler can be started
/// after shutdown), and registry/settings/credential commands keep working (the app drops the engine anyway).
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_is_idempotent_and_leaves_a_sessionless_engine() {
    let fresh = engine_in_memory(options());
    step("shutdown before start", fresh.engine.shutdown()).await.unwrap();
    assert_eq!(fresh.engine.start().await.err(), Some(GitYasError::Cancelled));
    assert!(fresh.sink.take().is_empty());

    let h = engine_in_memory(options());
    let dir = tempfile::tempdir().unwrap();
    let root = init_repo(dir.path());
    let repo = h.engine.add_repository(root, None).await.unwrap();
    h.engine.open_repository(repo.id.clone()).await.unwrap();
    h.sink.take();

    step("shutdown", h.engine.shutdown()).await.unwrap();
    step("shutdown again", h.engine.shutdown()).await.unwrap();
    let clone = h.engine.clone();
    step("shutdown via a clone", clone.shutdown()).await.unwrap();
    assert!(h.sink.take().is_empty(), "shutdown emits nothing");
    assert_eq!(h.engine.active_repository(), None);

    let none = Some(GitYasError::NoActiveRepository);
    assert_eq!(h.engine.file_state().await.err(), none);
    assert_eq!(h.engine.head().await.err(), none);
    assert_eq!(h.engine.refresh(Refresh::ALL).await.err(), none);
    assert_eq!(h.engine.refresh_commits().await.err(), none);
    assert_eq!(h.engine.fetch(None).await.err(), none);
    assert_eq!(h.engine.commit_staged_files("x".into(), None).await.err(), none);
    assert_eq!(h.engine.test_connection(Provider::Github).await.err(), none);
    assert_eq!(h.engine.cancel_remote_operation(), 0);
    step("close without session", h.engine.close_repository()).await.unwrap();

    // No new session.
    let cancelled = Some(GitYasError::Cancelled);
    assert_eq!(h.engine.open_repository(repo.id.clone()).await.err(), cancelled);
    assert_eq!(h.engine.start().await.err(), cancelled);
    assert_eq!(h.engine.active_repository(), None);
    assert!(h.sink.take().is_empty(), "nothing emitted after shutdown");

    // Global state is still readable and writable, and the tab survives for the next start.
    assert_eq!(h.engine.repositories().await.unwrap().len(), 1);
    h.engine.settings().await.unwrap();
    assert_eq!(h.engine.open_tabs().await.unwrap().active, Some(repo.id.clone()));
    h.engine
        .update_commit_signature(CommitSignature::new("Ada", "ada@example.com"))
        .await
        .unwrap();
    assert_eq!(names(&h.sink.take()), ["CommitsSignatureUpdated"]);
}

/// Scenario 7, race (engine bug WP8f.1): a switch that is still pending when `shutdown` runs must not install a
/// session (watchers + fetch scheduler) after `shutdown` returned.
#[tokio::test(flavor = "multi_thread")]
async fn a_switch_pending_during_shutdown_does_not_start_a_session() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..40 {
        let h = engine_in_memory(EngineOptions {
            fetch_interval: Some(std::time::Duration::from_millis(5)),
            ..options()
        });
        let a = h
            .engine
            .add_repository(init_repo(&dir.path().join(format!("a{i}"))), None)
            .await
            .unwrap();
        let b = h
            .engine
            .add_repository(init_repo(&dir.path().join(format!("b{i}"))), None)
            .await
            .unwrap();
        h.engine.open_repository(a.id.clone()).await.unwrap();
        let open = tokio::spawn({
            let engine = h.engine.clone();
            async move { engine.open_repository(b.id).await }
        });
        if i % 2 == 0 {
            tokio::task::yield_now().await;
        }
        step("shutdown", h.engine.shutdown()).await.unwrap();
        let opened = step("pending open", open).await.unwrap();
        assert!(matches!(opened, Ok(()) | Err(GitYasError::Cancelled)), "{opened:?}");
        assert_eq!(h.engine.active_repository(), None, "round {i}: a session outlived shutdown");
        h.sink.take();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(h.sink.take().is_empty(), "round {i}: events after shutdown");
    }
}

/// Scenario 7, in flight: shutdown aborts a running fetch (gated in its first progress report) without deadlock.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_aborts_an_in_flight_fetch() {
    let h = engine_in_memory(options());
    let dir = tempfile::tempdir().unwrap();
    let remote = bare_remote(&dir.path().join("remote"), 150, 8);
    let root = init_repo(&dir.path().join("a"));
    add_origin(&root, &remote);
    let repo = h.engine.add_repository(root, None).await.unwrap();
    h.engine.open_repository(repo.id.clone()).await.unwrap();
    h.sink.take();

    h.sink.block_progress_of(&repo.id);
    let fetch = tokio::spawn({
        let engine = h.engine.clone();
        async move { engine.fetch(None).await }
    });
    let sink = h.sink.clone();
    let blocked = tokio::task::spawn_blocking(move || sink.wait_blocked(STEP)).await.unwrap();
    assert!(blocked, "the fetch reported no progress");

    let shutdown = tokio::spawn({
        let engine = h.engine.clone();
        async move { engine.shutdown().await }
    });
    // `shutdown` signals the remote op before it waits for the command lock the fetch holds.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!shutdown.is_finished(), "shutdown must wait for the command holding the lock");
    h.sink.release();

    let fetched = step("fetch", fetch).await.unwrap();
    step("shutdown", shutdown).await.unwrap().unwrap();
    assert!(matches!(fetched, Err(GitYasError::Cancelled) | Ok(_)), "{fetched:?}");
    let after: Vec<_> = h
        .sink
        .take()
        .into_iter()
        .filter(|ev| !matches!(ev, AppEvent::RemoteProgress { .. }))
        .collect();
    assert!(after.is_empty(), "no state events from the shut-down session: {:?}", names(&after));
    assert_eq!(h.engine.active_repository(), None);
}

/// Scenario 5, in flight: deleting the active repository while its fetch is running cancels the fetch, and no
/// state event of the deleted repository follows the deselection.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_the_active_repository_mid_fetch_cancels_it() {
    let h = engine_in_memory(options());
    let dir = tempfile::tempdir().unwrap();
    let remote = bare_remote(&dir.path().join("remote"), 150, 8);
    let root = init_repo(&dir.path().join("a"));
    add_origin(&root, &remote);
    let repo = h.engine.add_repository(root, None).await.unwrap();
    h.engine.open_repository(repo.id.clone()).await.unwrap();
    h.sink.take();

    h.sink.block_progress_of(&repo.id);
    let fetch = tokio::spawn({
        let engine = h.engine.clone();
        async move { engine.fetch(None).await }
    });
    let sink = h.sink.clone();
    assert!(tokio::task::spawn_blocking(move || sink.wait_blocked(STEP)).await.unwrap());
    let delete = tokio::spawn({
        let engine = h.engine.clone();
        let id = repo.id.clone();
        async move { engine.delete_repository(id).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!delete.is_finished());
    h.sink.release();

    assert_eq!(step("fetch", fetch).await.unwrap().err(), Some(GitYasError::Cancelled));
    step("delete", delete).await.unwrap().unwrap();
    let events = h.sink.take();
    let sel = position(&events, |ev| *ev == AppEvent::SelectedRepositoryUpdated(None)).unwrap();
    assert!(events[sel..].iter().all(|ev| ev.repo().is_none()), "{:?}", names(&events));
    assert_eq!(names(&events[sel..]), ["SelectedRepositoryUpdated", "RepositoriesUpdated"]);
    assert_eq!(h.engine.active_repository(), None);
    assert!(h.engine.repositories().await.unwrap().is_empty());
}
