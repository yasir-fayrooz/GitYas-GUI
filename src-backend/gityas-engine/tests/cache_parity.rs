//! Scenario 4: the commit-graph cache is invisible in the event stream. Cold fill, warm hit (after a restart on
//! the same cache dir), the in-memory incremental path and the SQLite incremental path (mutation while the engine is
//! down) must produce exactly the `CommitWrapper` of a live walk (`use_commit_cache: false`).

// Test helpers outside `#[test]` fns: `allow-unwrap-in-tests` does not reach them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::*;
use gityas_core::models::{CommitSignature, CommitWrapper};
use gityas_core::{AppEvent, RepoId};
use gityas_engine::EngineOptions;

fn cached() -> EngineOptions {
    EngineOptions {
        use_commit_cache: true,
        ..options()
    }
}

fn live() -> EngineOptions {
    EngineOptions {
        use_commit_cache: false,
        ..options()
    }
}

/// History with branches, a merge, a lightweight and an annotated tag, and a stash.
fn fixture(dir: &Path) -> std::path::PathBuf {
    let root = init_repo(dir);
    let repo = git2::Repository::open(&root).unwrap();
    let sig = signature();
    for i in 0..5 {
        commit_file(&root, &format!("main{i}.txt"), &format!("{i}\n"), &format!("main {i}"));
    }
    let fork = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature", &fork, false).unwrap();
    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force())).unwrap();
    for i in 0..3 {
        commit_file(&root, &format!("feat{i}.txt"), &format!("{i}\n"), &format!("feature {i}"));
    }
    let feature_tip = repo.head().unwrap().peel_to_commit().unwrap();
    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force())).unwrap();
    commit_file(&root, "main-after.txt", "x\n", "main after fork");
    let main_tip = repo.head().unwrap().peel_to_commit().unwrap();

    // Merge feature into main (no conflicts: disjoint files).
    let mut index = repo.merge_commits(&main_tip, &feature_tip, None).unwrap();
    let tree = repo.find_tree(index.write_tree_to(&repo).unwrap()).unwrap();
    let merge = repo
        .commit(Some("HEAD"), &sig, &sig, "Merge feature", &tree, &[&main_tip, &feature_tip])
        .unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force())).unwrap();
    repo.tag_lightweight("light", &repo.find_object(fork.id(), None).unwrap(), false)
        .unwrap();
    repo.tag("v1.0", &repo.find_object(merge, None).unwrap(), &sig, "release", false)
        .unwrap();
    // A side branch that will be deleted later (its commits become unreachable).
    repo.branch("doomed", &fork, false).unwrap();
    repo.set_head("refs/heads/doomed").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force())).unwrap();
    commit_file(&root, "doomed.txt", "d\n", "doomed work");
    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force())).unwrap();
    // A stash (listed in references).
    std::fs::write(root.join("README.md"), "stashed change\n").unwrap();
    let mut stash_repo = git2::Repository::open(&root).unwrap();
    stash_repo.stash_save(&sig, "wip", None).unwrap();
    root
}

/// `CommitsUpdated` payload a fresh in-memory engine with the cache off emits on open.
async fn live_commits(root: &Path) -> CommitWrapper {
    let h = engine_in_memory(live());
    let repo = h.engine.add_repository(root.to_path_buf(), None).await.unwrap();
    step("open live", h.engine.open_repository(repo.id.clone())).await.unwrap();
    let commits = last_commits(&h.sink.take(), &repo.id).unwrap();
    assert_eq!(step("refresh_commits live", h.engine.refresh_commits()).await.unwrap(), commits);
    h.engine.shutdown().await.unwrap();
    commits
}

/// The latest `CommitsUpdated` in `events`. A watcher refresh from the previous step can land late (Windows delivers
/// file notifications after the command's own events), so there may be more than one.
fn only_commits(events: &[AppEvent], repo: &RepoId) -> CommitWrapper {
    last_commits(events, repo).unwrap_or_else(|| panic!("no CommitsUpdated in {:?}", names(events)))
}

#[tokio::test(flavor = "multi_thread")]
async fn cache_hit_miss_and_incremental_refreshes_equal_the_live_walk() {
    let repos = tempfile::tempdir().unwrap();
    let root = fixture(&repos.path().join("r"));
    let data = tempfile::tempdir().unwrap();
    let baseline = live_commits(&root).await;
    assert_eq!(
        baseline.commits.len(),
        12,
        "fixture: {:?}",
        baseline.commits.iter().map(|c| &c.summary).collect::<Vec<_>>()
    );

    // Cold: empty cache dir → full walk, stored.
    let id = {
        let h = engine_on_disk(data.path(), cached());
        let repo = h.engine.add_repository(root.clone(), None).await.unwrap();
        step("open cold", h.engine.open_repository(repo.id.clone())).await.unwrap();
        let cold = only_commits(&h.sink.take(), &repo.id);
        assert_eq!(cold, baseline, "cold fill differs from live walk");
        // Same refs again (hot entry).
        assert_eq!(step("hot", h.engine.refresh_commits()).await.unwrap(), baseline);
        h.engine.shutdown().await.unwrap();
        repo.id
    };
    assert_eq!(
        cached_history_len(data.path(), &id),
        Some(baseline.commits.len()),
        "cold open filled the cache"
    );

    // Warm: new engine, same cache dir → served from SQLite.
    {
        let h = engine_on_disk(data.path(), cached());
        step("start warm", h.engine.start()).await.unwrap();
        let warm = only_commits(&h.sink.take(), &id);
        assert_eq!(warm, baseline, "warm cache differs from live walk");

        // Mutation through the engine: a new commit (in-memory incremental path).
        std::fs::write(root.join("new.txt"), "n\n").unwrap();
        let state = h.engine.file_state().await.unwrap();
        h.engine.stage_files(state.unstaged).await.unwrap();
        h.engine
            .update_commit_signature(CommitSignature::new("Ada", "ada@example.com"))
            .await
            .unwrap();
        h.sink.take();
        let oid = step("commit", h.engine.commit_staged_files("engine commit".into(), None))
            .await
            .unwrap();
        let after_commit = only_commits(&h.sink.take(), &id);
        assert_eq!(after_commit.commits[0].id, oid);
        assert_eq!(after_commit, live_commits(&root).await, "after commit");

        // Branch delete: `doomed`'s commit becomes unreachable and must disappear.
        step("delete branch", h.engine.delete_branch("refs/heads/doomed".into(), true))
            .await
            .unwrap();
        let after_delete = only_commits(&h.sink.take(), &id);
        assert!(after_delete.commits.iter().all(|c| c.summary != "doomed work"));
        assert_eq!(after_delete.commits.len(), after_commit.commits.len() - 1);
        assert_eq!(after_delete, live_commits(&root).await, "after branch delete");

        // Tag delete (refs only).
        step("delete tag", h.engine.delete_tag("light".into())).await.unwrap();
        assert_eq!(only_commits(&h.sink.take(), &id), live_commits(&root).await, "after tag delete");
        h.engine.shutdown().await.unwrap();
        assert_eq!(cached_history_len(data.path(), &id), Some(after_delete.commits.len()));
    }

    // Mutations while the engine is down (SQLite incremental path on restart): new commits, a reset that drops one
    // of them again, and a new branch.
    let a = commit_file(&root, "offline1.txt", "1\n", "offline 1");
    commit_file(&root, "offline2.txt", "2\n", "offline 2");
    {
        let repo = git2::Repository::open(&root).unwrap();
        let target = repo.find_object(a, None).unwrap();
        repo.reset(&target, git2::ResetType::Hard, None).unwrap();
        repo.branch("later", &repo.find_commit(a).unwrap(), false).unwrap();
    }
    let h = engine_on_disk(data.path(), cached());
    step("start after offline changes", h.engine.start()).await.unwrap();
    let restarted = only_commits(&h.sink.take(), &id);
    assert_eq!(restarted, live_commits(&root).await, "after offline mutation + restart");
    assert!(restarted.commits.iter().all(|c| c.summary != "offline 2"));
    h.engine.shutdown().await.unwrap();

    // And a third engine with the cache off on the same dirs agrees too (the flag only bypasses the cache).
    let h = engine_on_disk(data.path(), live());
    step("start live on disk", h.engine.start()).await.unwrap();
    assert_eq!(only_commits(&h.sink.take(), &id), restarted);
    h.engine.shutdown().await.unwrap();
}

/// Two repositories sharing one cache DB never see each other's history, also across switches.
#[tokio::test(flavor = "multi_thread")]
async fn switching_between_cached_repositories_keeps_histories_apart() {
    let repos = tempfile::tempdir().unwrap();
    let a_root = fixture(&repos.path().join("a"));
    let b_root = init_repo(&repos.path().join("b"));
    for i in 0..4 {
        commit_file(&b_root, &format!("b{i}"), "b\n", &format!("b {i}"));
    }
    let (a_live, b_live) = (live_commits(&a_root).await, live_commits(&b_root).await);
    let data = tempfile::tempdir().unwrap();
    let h = engine_on_disk(data.path(), cached());
    let a = h.engine.add_repository(a_root, None).await.unwrap();
    let b = h.engine.add_repository(b_root, None).await.unwrap();
    for round in 0..3 {
        for (repo, expected) in [(&a, &a_live), (&b, &b_live)] {
            h.sink.take();
            step("open", h.engine.open_repository(repo.id.clone())).await.unwrap();
            let events = h.sink.take();
            assert_only_repo(&events, &repo.id);
            assert_eq!(&only_commits(&events, &repo.id), expected, "round {round}, {}", repo.name);
        }
    }
    h.engine.shutdown().await.unwrap();
}

/// `commit_files` (cached on first use) equals the uncached answer, cold, warm and after a restart, including the
/// root commit and a merge (first-parent diff).
#[tokio::test(flavor = "multi_thread")]
async fn commit_files_from_the_cache_equal_the_live_diff() {
    let repos = tempfile::tempdir().unwrap();
    let root = fixture(&repos.path().join("r"));
    let data = tempfile::tempdir().unwrap();

    let live = engine_in_memory(live());
    let id = live.engine.add_repository(root.clone(), None).await.unwrap().id;
    live.engine.open_repository(id.clone()).await.unwrap();
    let history = live.engine.refresh_commits().await.unwrap();
    let mut expected = Vec::new();
    for commit in history.commits.iter() {
        expected.push(step("live files", live.engine.commit_files(commit.id.clone())).await.unwrap());
    }
    assert!(
        expected.iter().all(|files| !files.is_empty()),
        "every fixture commit changes a file"
    );
    live.engine.shutdown().await.unwrap();

    for round in ["cold", "warm", "after restart"] {
        let h = engine_on_disk(data.path(), cached());
        if round == "cold" {
            let repo = h.engine.add_repository(root.clone(), None).await.unwrap();
            h.engine.open_repository(repo.id).await.unwrap();
        } else {
            h.engine.start().await.unwrap();
        }
        for _ in 0..2 {
            for (commit, want) in history.commits.iter().zip(&expected) {
                let got = step("cached files", h.engine.commit_files(commit.id.clone())).await.unwrap();
                assert_eq!(&got, want, "{round}: {}", commit.summary);
            }
        }
        h.engine.shutdown().await.unwrap();
    }
}
