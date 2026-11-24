//! History loading on a real repository: libgit2 (the previous implementation, kept here as the baseline) against
//! the gitoxide paths. Read-only on the repository given; the commit-graph case runs on a temporary local clone.
//!
//! ```text
//! GITYAS_BENCH_REPO=path/to/large/repo cargo test -p gityas-git --release --test history_bench -- --ignored --nocapture
//! ```
//!
//! Every sample opens fresh handles, as the engine does per load (no warm object caches across samples).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gityas_core::models::Commit;
use gityas_git::commits::{commit_row, sort_commits};
use gityas_git::gitoxide;

fn best<T>(k: usize, mut f: impl FnMut() -> T) -> (T, Duration) {
    let mut out = None;
    let mut min = Duration::MAX;
    for _ in 0..k {
        let start = Instant::now();
        let o = f();
        min = min.min(start.elapsed());
        out = Some(o);
    }
    (out.unwrap(), min)
}

fn ms(d: Duration) -> String {
    format!("{:>8.1} ms", d.as_secs_f64() * 1000.0)
}

/// The libgit2 tip set of the previous `cache::ref_tips`.
fn git2_tips(repo: &git2::Repository) -> Vec<git2::Oid> {
    let mut tips = Vec::new();
    for r in repo.references().unwrap() {
        let r = r.unwrap();
        let Ok(name) = std::str::from_utf8(r.name_bytes()) else { continue };
        if !["refs/heads/", "refs/remotes/", "refs/tags/"].iter().any(|ns| name.starts_with(ns)) {
            continue;
        }
        if let Ok(c) = r.peel_to_commit() {
            tips.push(c.id());
        }
    }
    if let Ok(c) = repo.head().and_then(|h| h.peel_to_commit()) {
        tips.push(c.id());
    }
    tips.sort();
    tips.dedup();
    tips
}

/// The previous `commits::get_commits_live` (libgit2 revwalk + `find_commit` + row).
fn git2_live(path: &Path) -> Vec<Commit> {
    let repo = git2::Repository::open(path).unwrap();
    let mut walk = repo.revwalk().unwrap();
    for tip in git2_tips(&repo) {
        walk.push(tip).unwrap();
    }
    let mut rows = Vec::new();
    for oid in walk {
        rows.push(commit_row(&repo.find_commit(oid.unwrap()).unwrap()).unwrap());
    }
    sort_commits(rows)
}

/// The previous `commits::get_commits_newest` (own heap over libgit2 lookups).
fn git2_newest(path: &Path, max: usize) -> usize {
    let repo = git2::Repository::open(path).unwrap();
    let mut queue = std::collections::BinaryHeap::new();
    let mut seen = HashSet::new();
    for tip in git2_tips(&repo) {
        if seen.insert(tip) {
            queue.push((repo.find_commit(tip).unwrap().time().seconds(), tip));
        }
    }
    let mut rows = Vec::new();
    while let Some((_, oid)) = queue.pop() {
        if rows.len() == max {
            break;
        }
        let commit = repo.find_commit(oid).unwrap();
        for p in commit.parent_ids() {
            if seen.insert(p)
                && let Ok(pc) = repo.find_commit(p)
            {
                queue.push((pc.time().seconds(), p));
            }
        }
        rows.push(commit_row(&commit).unwrap());
    }
    sort_commits(rows).len()
}

fn gix_ids(repo: &gix::Repository, use_graph: bool) -> Vec<gix::ObjectId> {
    let tips: Vec<_> = gityas_git::cache::ref_tips(repo)
        .unwrap()
        .iter()
        .map(|t| gitoxide::oid_to_gix(&t.oid).unwrap())
        .collect();
    repo.rev_walk(tips)
        .use_commit_graph(use_graph)
        .all()
        .unwrap()
        .map(|i| i.unwrap().id)
        .collect()
}

/// Equal histories, or a short report of the difference (ids only in one side, first differing row).
fn assert_same(got: &[Commit], want: &[Commit]) {
    if got == want {
        return;
    }
    let got_ids: HashSet<_> = got.iter().map(|c| &c.id).collect();
    let want_ids: HashSet<_> = want.iter().map(|c| &c.id).collect();
    let only_got: Vec<_> = got_ids.difference(&want_ids).take(5).collect();
    let only_want: Vec<_> = want_ids.difference(&got_ids).take(5).collect();
    let first = got.iter().zip(want).position(|(a, b)| a != b);
    let detail = first.map(|i| {
        format!(
            "first difference at row {i}:
  gitoxide {:?}
  libgit2  {:?}",
            got[i], want[i]
        )
    });
    panic!(
        "histories differ: {} vs {} commits; only gitoxide {only_got:?}; only libgit2 {only_want:?}; {}",
        got.len(),
        want.len(),
        detail.unwrap_or_default()
    );
}

#[test]
#[ignore = "benchmark; set GITYAS_BENCH_REPO and run with --release --ignored --nocapture"]
fn history_benchmark() {
    let Some(path) = std::env::var_os("GITYAS_BENCH_REPO").map(PathBuf::from) else {
        eprintln!("GITYAS_BENCH_REPO is not set; skipping");
        return;
    };
    let page = gityas_core::models::GraphOptions::default().max_commits;

    // Refs on the repository itself (loose vs packed refs matter; timing only, so concurrent fetches are harmless).
    let open_orig = || gitoxide::open(&path).unwrap();
    let (_, git2_tips_time) = best(5, || git2_tips(&git2::Repository::open(&path).unwrap()));
    let (_, gix_tips_time) = best(5, || gityas_git::cache::ref_tips(&open_orig()).unwrap());
    let (_, open_time) = best(5, open_orig);
    let (ref_count, refs_read_time) = best(5, || {
        let repo = open_orig();
        let platform = repo.references().unwrap();
        platform
            .all()
            .unwrap()
            .filter_map(Result::ok)
            .filter(|r| r.try_id().is_some())
            .count()
    });
    let (_, refs_header_time) = best(5, || {
        let repo = open_orig();
        let platform = repo.references().unwrap();
        let ids: Vec<_> = platform
            .all()
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|r| r.try_id().map(|id| id.detach()))
            .collect();
        ids.iter()
            .filter(|id| repo.find_header(**id).is_ok_and(|h| h.kind() == gix::object::Kind::Commit))
            .count()
    });

    // History on a frozen mirror (objects hard-linked; the original may be fetched into meanwhile), first without,
    // then with a commit-graph file.
    let tmp = tempfile::TempDir::new().unwrap();
    let clone = tmp.path().join("clone.git");
    let git = |args: &[&str], dir: &Path| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .is_ok_and(|s| s.success())
    };
    assert!(
        git(
            &[
                "clone",
                "--quiet",
                "--mirror",
                "--local",
                &path.to_string_lossy(),
                &clone.to_string_lossy()
            ],
            tmp.path()
        ),
        "git clone --mirror failed"
    );
    let open = || gitoxide::open(&clone).unwrap();
    let (baseline, git2_live_time) = best(3, || git2_live(&clone));
    let n = baseline.len();
    let (_, git2_page_time) = best(5, || git2_newest(&clone, page));
    let (live, gix_live_time) = best(3, || gityas_git::commits::get_commits_live(&open()).unwrap());
    assert_same(&live, &baseline);
    let (_, gix_page_time) = best(5, || gityas_git::commits::get_commits_newest(&open(), page).unwrap());
    let (ids, walk_ids_time) = best(3, || gix_ids(&open(), false));
    let (_, decode_par_time) = best(3, || gitoxide::decode_rows(&open(), &ids).unwrap());
    let (_, decode_seq_time) = best(3, || {
        let repo = open();
        let mut buf = Vec::new();
        ids.iter().map(|id| gitoxide::read_row(&repo, id, &mut buf).unwrap()).count()
    });
    let (_, sort_time) = best(3, || sort_commits(live.clone()));

    let graph_lines = if git(&["commit-graph", "write", "--reachable"], &clone) {
        let (graph_live, graph_live_time) = best(3, || gityas_git::commits::get_commits_live(&open()).unwrap());
        assert_same(&graph_live, &baseline);
        let (_, graph_page_time) = best(5, || gityas_git::commits::get_commits_newest(&open(), page).unwrap());
        let (graph_ids, graph_walk_time) = best(3, || gix_ids(&open(), true));
        let (_, git2_graph_time) = best(3, || git2_live(&clone));
        format!(
            "  with a commit-graph file
    live walk          libgit2 {}   gitoxide {}
      graph walk (ids only)               {} ({} ids)
    gitoxide first page                   {}
",
            ms(git2_graph_time),
            ms(graph_live_time),
            ms(graph_walk_time),
            graph_ids.len(),
            ms(graph_page_time),
        )
    } else {
        "  (commit-graph case skipped: git commit-graph write failed)
"
        .to_owned()
    };

    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    println!(
        "history_benchmark: {} — {n} commits, {threads} threads; best of 3-5, fresh handles",
        path.display()
    );
    println!(
        "  live walk          libgit2 {}   gitoxide {}",
        ms(git2_live_time),
        ms(gix_live_time)
    );
    println!(
        "  first {page} page    libgit2 {}   gitoxide {}",
        ms(git2_page_time),
        ms(gix_page_time)
    );
    println!(
        "  ref tips           libgit2 {}   gitoxide {}",
        ms(git2_tips_time),
        ms(gix_tips_time)
    );
    println!("  gitoxide breakdown");
    println!("    open handle                           {}", ms(open_time));
    println!("    read {ref_count} refs (names + ids)       {}", ms(refs_read_time));
    println!("    read refs + object header kind        {}", ms(refs_header_time));
    println!("    rev walk, ids only (no graph)         {}", ms(walk_ids_time));
    println!("    decode rows, parallel                 {}", ms(decode_par_time));
    println!("    decode rows, one thread               {}", ms(decode_seq_time));
    println!("    sort_commits                          {}", ms(sort_time));
    print!("{graph_lines}");
}
