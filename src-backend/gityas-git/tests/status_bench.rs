//! Where working-tree status time goes on a real repository (read-only):
//!
//! ```text
//! GITYAS_BENCH_REPO=path/to/repo cargo test -p gityas-git --release --test status_bench -- --ignored --nocapture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

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

fn git2_statuses(path: &std::path::Path, renames: bool, untracked: bool) -> usize {
    let repo = git2::Repository::open(path).unwrap();
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(untracked)
        .recurse_untracked_dirs(untracked)
        .include_ignored(false)
        .renames_head_to_index(renames)
        .renames_index_to_workdir(renames)
        .renames_from_rewrites(renames);
    repo.statuses(Some(&mut opts)).unwrap().len()
}

fn gix_status(path: &std::path::Path, untracked: bool) -> usize {
    let repo = gix::open(path).unwrap();
    let platform = repo.status(gix::progress::Discard).unwrap().untracked_files(if untracked {
        gix::status::UntrackedFiles::Files
    } else {
        gix::status::UntrackedFiles::None
    });
    platform.into_iter(None).unwrap().filter_map(Result::ok).count()
}

#[test]
#[ignore = "benchmark; set GITYAS_BENCH_REPO and run with --release --ignored --nocapture"]
fn status_benchmark() {
    let Some(path) = std::env::var_os("GITYAS_BENCH_REPO").map(PathBuf::from) else {
        return;
    };
    let repo = git2::Repository::open(&path).unwrap();
    let files = repo.index().unwrap().len();
    let (old_state, current) = best(5, || gityas_git::status::get_status_libgit2(&repo).unwrap());
    let (new_state, gix_full) = best(5, || gityas_git::status::get_status(&gix::open(&path).unwrap()).unwrap());
    assert_eq!(new_state, old_state, "gitoxide status differs from libgit2's");
    let one = repo
        .index()
        .unwrap()
        .get(files / 2)
        .map(|e| PathBuf::from(String::from_utf8_lossy(&e.path).into_owned()));
    let one: Vec<PathBuf> = one.into_iter().collect();
    let (_, paths_time) = best(5, || {
        gityas_git::status::get_status_paths(&gix::open(&path).unwrap(), &one).unwrap()
    });
    let (_, plain) = best(5, || git2_statuses(&path, false, true));
    let (_, no_untracked) = best(5, || git2_statuses(&path, false, false));
    let (_, renames) = best(5, || git2_statuses(&path, true, true));
    let (_, gix_all) = best(5, || gix_status(&path, true));
    let (_, gix_tracked) = best(5, || gix_status(&path, false));
    let (_, cli) = best(5, || {
        std::process::Command::new("git")
            .args(["status", "--porcelain=v2", "-uall"])
            .current_dir(&path)
            .output()
            .unwrap()
    });
    println!("status_benchmark: {} — {files} index entries; best of 5", path.display());
    println!("  previous get_status (libgit2)           {}", ms(current));
    println!("  get_status (gitoxide, same result)      {}", ms(gix_full));
    println!("  get_status_paths, one file              {}", ms(paths_time));
    println!("    libgit2, renames on                   {}", ms(renames));
    println!("    libgit2, no renames                   {}", ms(plain));
    println!("    libgit2, no renames, no untracked     {}", ms(no_untracked));
    println!("  gitoxide status (tree+index+worktree)   {}", ms(gix_all));
    println!("    gitoxide, no untracked                {}", ms(gix_tracked));
    println!("  git CLI (status --porcelain=v2 -uall)   {}", ms(cli));
}
