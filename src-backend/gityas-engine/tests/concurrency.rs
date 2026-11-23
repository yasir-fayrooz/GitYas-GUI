//! Scenarios 2 and 8: switching repositories while remote operations and commands are in flight.

// Test helpers outside `#[test]` fns: `allow-unwrap-in-tests` does not reach them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use gityas_core::models::BranchKind;
use gityas_core::{AppEvent, GitYasError, RepoId};
use gityas_engine::{EngineOptions, Refresh};

/// Two registered repositories: `a` with an `origin` holding enough history that fetching it reports progress
/// several times, `b` without remotes.
struct Fixture {
    _dir: tempfile::TempDir,
    a: RepoId,
    a_root: std::path::PathBuf,
    b: RepoId,
}

async fn fixture(h: &Harness, remote_commits: usize) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let remote = bare_remote(&dir.path().join("remote"), remote_commits, 8);
    let a_root = init_repo(&dir.path().join("a"));
    add_origin(&a_root, &remote);
    let b_root = init_repo(&dir.path().join("b"));
    commit_file(&b_root, "b.txt", "b\n", "b only");
    let a = h.engine.add_repository(a_root.clone(), None).await.unwrap().id;
    let b = h.engine.add_repository(b_root, None).await.unwrap().id;
    Fixture { _dir: dir, a, a_root, b }
}

/// Events after the last `SelectedRepositoryUpdated(Some(id))`, and that id.
fn after_last_selection(events: &[AppEvent]) -> (RepoId, &[AppEvent]) {
    let i = events
        .iter()
        .rposition(|ev| matches!(ev, AppEvent::SelectedRepositoryUpdated(Some(_))))
        .unwrap_or_else(|| panic!("no selection in {:?}", names(events)));
    let AppEvent::SelectedRepositoryUpdated(Some(id)) = &events[i] else {
        unreachable!()
    };
    (id.clone(), &events[i + 1..])
}

/// Scenario 2: repo A's fetch is parked inside its first progress report; `open_repository(B)` starts meanwhile.
/// The switch cancels A's fetch (it returns `Cancelled`, or `Ok` if libgit2 had nothing left to report), nothing
/// A-scoped is emitted after the switch, and B's full state is.
#[tokio::test(flavor = "multi_thread")]
async fn switching_repository_mid_fetch_cancels_it_and_drops_its_events() {
    let h = engine_in_memory(options());
    let f = fixture(&h, 200).await;
    step("open a", h.engine.open_repository(f.a.clone())).await.unwrap();
    h.sink.take();

    h.sink.block_progress_of(&f.a);
    let started = tokio::time::Instant::now();
    let fetch = tokio::spawn({
        let engine = h.engine.clone();
        async move { engine.fetch(None).await }
    });
    let sink = h.sink.clone();
    assert!(
        tokio::task::spawn_blocking(move || sink.wait_blocked(STEP)).await.unwrap(),
        "the fetch reported no progress"
    );

    let switch = tokio::spawn({
        let engine = h.engine.clone();
        let b = f.b.clone();
        async move { engine.open_repository(b).await }
    });
    // Give `open_repository` time to validate B and cancel A's session; it then waits for the command lock that the
    // parked fetch holds. (No public signal for "cancel_active has run".)
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!switch.is_finished(), "the switch must wait for the command holding the lock");
    assert_eq!(
        h.engine.active_repository(),
        Some(f.a.clone()),
        "not switched before the lock is free"
    );
    h.sink.release();

    let fetched = step("fetch", fetch).await.unwrap();
    step("switch", switch).await.unwrap().unwrap();
    assert!(started.elapsed() < Duration::from_secs(10), "switch took {:?}", started.elapsed());
    assert_eq!(fetched.as_ref().err(), Some(&GitYasError::Cancelled), "{fetched:?}");
    assert_eq!(h.engine.active_repository(), Some(f.b.clone()));

    let events = h.sink.all();
    let sel = events
        .iter()
        .position(|ev| *ev == AppEvent::SelectedRepositoryUpdated(Some(f.b.clone())))
        .unwrap();
    // Before the switch: only A's progress (the parked report), nothing from B.
    assert_only_repo(&events[..sel], &f.a);
    // After the switch: B's full state and nothing for A.
    let after = &events[sel + 1..];
    assert_only_repo(after, &f.b);
    assert_subsequence(
        after,
        &[
            "CommitsUpdated",
            "CommitsHeadUpdated",
            "LocalBranchesUpdated",
            "RemoteBranchesUpdated",
            "RemotesUpdated",
            "GitFilesChanged",
        ],
    );
    assert_eq!(last_commits(after, &f.b).unwrap().commits[0].summary, "b only");

    // A cancelled fetch leaves A usable: reopen and fetch to completion.
    step("reopen a", h.engine.open_repository(f.a.clone())).await.unwrap();
    let summary = step("fetch a", h.engine.fetch(None)).await.unwrap();
    assert!(summary.updated_any());
    let remote = step("remote branches", h.engine.branches(BranchKind::Remote)).await.unwrap();
    assert!(remote.iter().any(|b| b.refname == "refs/remotes/origin/main"), "{remote:?}");
    h.engine.shutdown().await.unwrap();
}

/// Scenario 2, scheduler variant: background fetches every few ms on A while switching A ↔ B. No deadlock and no
/// A-scoped event once B is active.
#[tokio::test(flavor = "multi_thread")]
async fn switching_while_the_fetch_scheduler_runs_is_bounded_and_clean() {
    let h = engine_in_memory(EngineOptions {
        fetch_interval: Some(Duration::from_millis(5)),
        ..options()
    });
    let f = fixture(&h, 60).await;
    for _ in 0..5 {
        step("open a", h.engine.open_repository(f.a.clone())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await; // let a scheduled fetch start
        step("open b", h.engine.open_repository(f.b.clone())).await.unwrap();
    }
    // B has no remotes; give any straggler from A time to (wrongly) emit.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let events = h.sink.all();
    let (active, after) = after_last_selection(&events);
    assert_eq!(active, f.b);
    assert_only_repo(after, &f.b);

    // The scheduler really fetches A when left alone: remote-tracking refs appear and are pushed as events. A fetch
    // from the switching loop may already have created them (a fetch that finds nothing new emits nothing), so
    // drop them first; B is active, so nothing of A's is running.
    let a_repo = git2::Repository::open(&f.a_root).unwrap();
    for mut reference in a_repo.references_glob("refs/remotes/*").unwrap().flatten() {
        reference.delete().unwrap();
    }
    step("open a", h.engine.open_repository(f.a.clone())).await.unwrap();
    h.sink.take();
    let (events, fetched) = collect_until(
        &h.sink,
        STEP,
        |ev| matches!(ev, AppEvent::RemoteBranchesUpdated { branches, .. } if !branches.is_empty()),
    )
    .await;
    assert!(fetched, "no scheduled fetch within {STEP:?}: {:?}", names(&events));
    assert_only_repo(&events, &f.a);
    assert!(
        !events.iter().any(|ev| matches!(ev, AppEvent::RemoteProgress { .. })),
        "background fetches report no progress"
    );
    step("shutdown", h.engine.shutdown()).await.unwrap();
}

/// Scenario 8: many concurrent commands interleaved with A ↔ B switches. No panic, no deadlock, and every
/// repo-scoped event after the last switch belongs to the final active repository.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_commands_and_switches_smoke() {
    let h = engine_in_memory(options());
    let f = fixture(&h, 20).await;
    step("open a", h.engine.open_repository(f.a.clone())).await.unwrap();

    let deadline = Duration::from_secs(60);
    let mut tasks = tokio::task::JoinSet::new();
    for worker in 0..8 {
        let engine = h.engine.clone();
        tasks.spawn(async move {
            for i in 0..15 {
                let result = match (worker + i) % 6 {
                    0 => engine.file_state().await.map(drop),
                    1 => engine.branches(BranchKind::Local).await.map(drop),
                    2 => engine.refresh(Refresh::ALL).await,
                    3 => engine.refresh_commits().await.map(drop),
                    4 => engine.fetch(None).await.map(drop),
                    _ => engine.head().await.map(drop),
                };
                match result {
                    // A fetch aborted by a switch.
                    Ok(()) | Err(GitYasError::Cancelled) => {}
                    Err(err) => panic!("worker {worker} step {i}: {err:?}"),
                }
            }
        });
    }
    let switcher = {
        let engine = h.engine.clone();
        let (a, b) = (f.a.clone(), f.b.clone());
        tokio::spawn(async move {
            for i in 0..20 {
                let id = if i % 2 == 0 { b.clone() } else { a.clone() };
                engine.open_repository(id).await.unwrap();
                tokio::task::yield_now().await;
            }
        })
    };
    tokio::time::timeout(deadline, async {
        while let Some(joined) = tasks.join_next().await {
            joined.unwrap();
        }
        switcher.await.unwrap();
    })
    .await
    .expect("deadlock: commands and switches did not finish");

    // Last switch was to A (i = 19). Everything emitted after its selection event is A's.
    let events = h.sink.all();
    let (active, after) = after_last_selection(&events);
    assert_eq!(active, f.a);
    assert_eq!(h.engine.active_repository(), Some(f.a.clone()));
    assert_only_repo(after, &f.a);
    // Across the whole run: a selection is always followed by that repository's state before another repo's.
    let mut current: Option<RepoId> = None;
    for ev in &events {
        match ev {
            AppEvent::SelectedRepositoryUpdated(sel) => current = sel.clone(),
            ev => {
                if let Some(repo) = ev.repo() {
                    assert_eq!(
                        Some(repo),
                        current.as_ref(),
                        "{} emitted while another repo was selected",
                        ev.name()
                    );
                }
            }
        }
    }
    // The engine is still consistent.
    step("file_state", h.engine.file_state()).await.unwrap();
    step("shutdown", h.engine.shutdown()).await.unwrap();
}
