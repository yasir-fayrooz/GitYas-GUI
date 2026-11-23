//! §3.4 #8 performance gate. `criterion` is not available offline, so this is an ignored test that prints timings:
//!
//! ```text
//! cargo test -p gityas-engine --release --offline -- --ignored --nocapture cache_benchmark
//! ```
//!
//! `GITYAS_BENCH_COMMITS` (default 100 000) sets the history size. The history is generated through a libgit2
//! mempack backend dumped into one packfile, so generation takes seconds, not minutes.

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use git2::{Oid, Repository, Signature, Time};
use gityas_storage::{Storage, StoragePaths};
use tempfile::TempDir;

use super::{Source, load_commits, load_history};
use crate::session::SessionCtx;
use crate::{Engine, EngineOptions};

/// Mainline with a 3-commit side branch merged every 50 commits; tags every 1000; a few branches and remotes.
fn generate(repo: &Repository, total: usize) -> Oid {
    let odb = repo.odb().unwrap();
    let mempack = odb.add_new_mempack_backend(1000).unwrap();
    let tree = repo.treebuilder(None).unwrap().write().unwrap();
    let tree = repo.find_tree(tree).unwrap();
    let mut time = 1_500_000_000i64;
    let mut make = |parents: &[Oid], n: usize| -> Oid {
        time += 30;
        let sig = Signature::new("Bench Author", "bench@example.com", &Time::new(time, 0)).unwrap();
        let parents: Vec<git2::Commit<'_>> = parents.iter().map(|p| repo.find_commit(*p).unwrap()).collect();
        let refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
        repo.commit(None, &sig, &sig, &format!("synthetic commit number {n}"), &tree, &refs)
            .unwrap()
    };
    let mut n = 0usize;
    let mut main = make(&[], n);
    let mut tags = Vec::new();
    while n + 1 < total {
        if n % 50 == 49 && n + 5 < total {
            let s1 = make(&[main], n + 1);
            let s2 = make(&[s1], n + 2);
            let s3 = make(&[s2], n + 3);
            main = make(&[main, s3], n + 4);
            n += 4;
        } else {
            n += 1;
            main = make(&[main], n);
        }
        if n / 1000 > tags.len() {
            tags.push((n, main));
        }
    }
    let mut buf = git2::Buf::new();
    mempack.dump(repo, &mut buf).unwrap();
    let mut writer = odb.packwriter().unwrap();
    writer.write_all(&buf).unwrap();
    writer.commit().unwrap();
    mempack.reset().unwrap();

    repo.reference("refs/heads/main", main, true, "bench").unwrap();
    repo.reference("refs/remotes/origin/main", main, true, "bench").unwrap();
    for (i, (n, oid)) in tags.iter().enumerate() {
        repo.reference(&format!("refs/tags/v{n}"), *oid, true, "bench").unwrap();
        if i % 20 == 0 {
            repo.reference(&format!("refs/heads/release-{n}"), *oid, true, "bench").unwrap();
        }
    }
    repo.set_head("refs/heads/main").unwrap();
    main
}

fn engine(storage: &Storage) -> Engine {
    Engine::with_options(
        storage.clone(),
        Arc::new(gityas_auth::MemoryCredentialStore::new()),
        Arc::new(gityas_core::events::RecordingSink::new()),
        tokio::runtime::Handle::current(),
        EngineOptions::default(),
    )
}

fn time<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let out = f();
    (out, start.elapsed())
}

/// Minimum over `k` runs (the machine is shared; the minimum is the least noisy estimate).
fn best<T>(k: usize, mut f: impl FnMut() -> T) -> (T, Duration) {
    let (mut out, mut min) = time(&mut f);
    for _ in 1..k {
        let (o, d) = time(&mut f);
        if d < min {
            min = d;
        }
        out = o;
    }
    (out, min)
}

fn ms(d: Duration) -> String {
    format!("{:>8.1} ms", d.as_secs_f64() * 1000.0)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn cache_benchmark() {
    let total: usize = std::env::var("GITYAS_BENCH_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let dir = TempDir::new().unwrap();
    let repo_path = dir.path().join("repo");
    let repo = Repository::init_bare(&repo_path).unwrap();
    let (_main, gen_time) = time(|| generate(&repo, total));
    drop(repo);

    let paths = StoragePaths {
        data_dir: dir.path().join("data"),
        cache_dir: dir.path().join("cache"),
    };
    let (storage, _) = Storage::open(&paths).unwrap();
    let repo_id = storage.user().insert_repository("bench", &repo_path, None).unwrap().id;
    let first = engine(&storage);
    let ctx: SessionCtx = first.inner.sessions.new_ctx(repo_id, repo_path.clone(), None);
    let repo = gityas_git::repository::open(&repo_path).unwrap();

    let (live, live_time) = best(3, || gityas_git::commits::get_commits_live(&repo).unwrap());
    let page = gityas_core::models::GraphOptions::default().max_commits;
    let (_, page_time) = best(5, || gityas_git::commits::get_commits_newest(&repo, page).unwrap());
    let n = live.len();
    let ((cold, src), cold_time) = time(|| load_history(&first.inner, &ctx, &repo).unwrap());
    assert_eq!(src, Source::ColdFill);
    assert_eq!(&cold[..], &live[..]);
    let ((_, src), hot_time) = best(5, || load_history(&first.inner, &ctx, &repo).unwrap());
    assert_eq!(src, Source::Hot);
    drop(first);
    drop(storage);

    // "Second open": a new process (storage reopened from disk, no hot entry), unchanged refs. Each sample uses a
    // fresh engine so it is a real SQLite hit.
    let (storage, _) = Storage::open(&paths).unwrap();
    let repo = gityas_git::repository::open(&repo_path).unwrap();
    let ((warm, src), warm_time) = best(5, || load_history(&engine(&storage).inner, &ctx, &repo).unwrap());
    assert_eq!(src, Source::Hit);
    assert_eq!(&warm[..], &live[..]);
    let (_, warm_wrapper_time) = best(5, || load_commits(&engine(&storage).inner, &ctx, &repo).unwrap());
    // Breakdown of the warm path.
    let (tips, tips_time) = best(5, || gityas_git::cache::ref_tips(&repo).unwrap());
    let tip_oids: Vec<_> = tips.iter().map(|t| t.oid.clone()).collect();
    let (_, snap_time) = best(5, || storage.cache().snapshot(&ctx.repo_id).unwrap());
    let (graph, load_time) = best(5, || storage.cache().load_reachable(&ctx.repo_id, &tip_oids).unwrap());
    let (_, sort_time) = best(5, || gityas_git::commits::sort_commits(graph.commits.clone()));
    let second = engine(&storage);
    load_history(&second.inner, &ctx, &repo).unwrap();
    let (_, hot_wrapper_time) = best(5, || load_commits(&second.inner, &ctx, &repo).unwrap());

    // Incremental refresh after one new commit on main. The engine's first store runs gc; later ones within
    // `cache_gc_interval` are throttled.
    let git = Repository::open_bare(&repo_path).unwrap();
    let mut clock = 2_000_000_000;
    let mut add_commit = || {
        clock += 1;
        let sig = Signature::new("Bench Author", "bench@example.com", &Time::new(clock, 0)).unwrap();
        let head = git.head().unwrap().peel_to_commit().unwrap();
        git.commit(Some("refs/heads/main"), &sig, &sig, "one more", &head.tree().unwrap(), &[&head])
            .unwrap();
    };
    let third = engine(&storage);
    add_commit();
    let ((inc, src), inc_gc_time) = time(|| load_history(&third.inner, &ctx, &repo).unwrap());
    assert_eq!(src, Source::Incremental);
    assert_eq!(inc.len(), n + 1);
    assert_eq!(third.inner.commit_graph.stats.gc_runs.load(std::sync::atomic::Ordering::SeqCst), 1);
    let mut inc_samples = Vec::new();
    for _ in 0..5 {
        add_commit();
        let ((_, src), d) = time(|| load_history(&third.inner, &ctx, &repo).unwrap());
        assert_eq!(src, Source::Incremental);
        inc_samples.push(d);
    }
    let inc_time = inc_samples.iter().copied().min().unwrap_or_default();
    assert_eq!(
        third
            .inner
            .commit_graph
            .stats
            .sqlite_loads
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let mut inc_sqlite_samples = Vec::new();
    for _ in 0..3 {
        add_commit();
        super::hot_clear(&third.inner.commit_graph, None);
        let ((_, src), d) = time(|| load_history(&third.inner, &ctx, &repo).unwrap());
        assert_eq!(src, Source::Incremental);
        inc_sqlite_samples.push(d);
    }
    let inc_sqlite_time = inc_sqlite_samples.iter().copied().min().unwrap_or_default();
    // Breakdown of one incremental step.
    let (snap, _) = time(|| storage.cache().snapshot(&ctx.repo_id).unwrap().unwrap());
    add_commit();
    let new_tips = gityas_git::cache::ref_tips(&repo).unwrap();
    let (rows, all_tips_walk_time) = time(|| gityas_git::cache::walk_new_commits(&repo, &new_tips, &snap.1).unwrap());
    assert_eq!(rows.len(), 1);
    let changed: Vec<_> = new_tips
        .iter()
        .filter(|t| !snap.1.iter().any(|o| o.oid == t.oid))
        .cloned()
        .collect();
    let (rows, inc_walk_time) = time(|| gityas_git::cache::walk_new_commits(&repo, &changed, &snap.1).unwrap());
    assert_eq!(rows.len(), 1);
    let known: std::collections::HashSet<&gityas_core::Oid> = live.iter().map(|c| &c.id).collect();
    let (_, known_set_time) = time(|| live.iter().map(|c| &c.id).collect::<std::collections::HashSet<_>>());
    let state = super::CommitGraphState::default();
    let (_, walk_known_time) = time(|| super::walk_until_known(&state, &repo, &new_tips, &known).unwrap());
    let fp = gityas_git::cache::fingerprint(&new_tips);
    let (_, inc_store_time) = time(|| storage.cache().store_refresh(&ctx.repo_id, &rows, &new_tips, &fp).unwrap());
    let (_, gc_time) = time(|| storage.cache().gc(&ctx.repo_id, 30 * 24 * 3600, 512 * 1024 * 1024).unwrap());
    // Breakdown of the cold fill (fresh repo id in the same DB).
    let cold_id = "bench-cold".to_owned();
    let (all_rows, cold_walk_time) = time(|| gityas_git::cache::walk_new_commits(&repo, &new_tips, &[]).unwrap());
    let (_, cold_store_time) = time(|| storage.cache().store_refresh(&cold_id, &all_rows, &new_tips, &fp).unwrap());
    let (_, cold_gc_time) = time(|| storage.cache().gc(&cold_id, 30 * 24 * 3600, 512 * 1024 * 1024).unwrap());
    let (_, cold_sort_time) = time(|| gityas_git::commits::sort_commits(all_rows));
    storage.cache().purge_repository(&cold_id).unwrap();
    let cache_bytes = std::fs::metadata(storage.cache().path().unwrap()).map(|m| m.len()).unwrap_or(0);

    println!(
        "cache_benchmark: {n} commits, {} tips (generated in {}); best of 3-5 samples",
        tips.len(),
        ms(gen_time)
    );
    println!("  live walk (get_commits_live)            {}", ms(live_time));
    println!("  first page ({page} newest, preview)      {}", ms(page_time));
    println!("  cold fill (walk + store + gc + sort)    {}", ms(cold_time));
    println!("    walk_new_commits (full)               {}", ms(cold_walk_time));
    println!("    store_refresh (100k rows)             {}", ms(cold_store_time));
    println!("    gc                                    {}", ms(cold_gc_time));
    println!("    sort_commits                          {}", ms(cold_sort_time));
    println!("  hot repeat (same engine, history only)  {}", ms(hot_time));
    println!("  hot repeat, full CommitWrapper          {}", ms(hot_wrapper_time));
    println!("  warm second open (SQLite hit), history  {}", ms(warm_time));
    println!("  warm second open, full CommitWrapper    {}", ms(warm_wrapper_time));
    println!("    ref_tips                              {}", ms(tips_time));
    println!("    snapshot                              {}", ms(snap_time));
    println!("    load_reachable                        {}", ms(load_time));
    println!("    sort_commits                          {}", ms(sort_time));
    println!("  incremental +1 commit, SQLite + gc      {}", ms(inc_gc_time));
    println!("  incremental +1 commit, hot (no gc)      {}", ms(inc_time));
    println!("  incremental +1 commit, via SQLite       {}", ms(inc_sqlite_time));
    println!("    known-id set (hot path)               {}", ms(known_set_time));
    println!("    walk_until_known (hot path)           {}", ms(walk_known_time));
    println!("    walk_new_commits (changed tips)       {}", ms(inc_walk_time));
    println!("    walk_new_commits (all tips pushed)    {}", ms(all_tips_walk_time));
    println!("    store_refresh                         {}", ms(inc_store_time));
    println!("    gc                                    {}", ms(gc_time));
    println!("  cache.db size                           {:>8.1} MB", cache_bytes as f64 / 1e6);
}
