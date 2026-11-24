//! The working-tree status the engine emits (`GitFilesChanged`), kept for the active session and updated from file
//! watcher batches without rescanning the whole tree.
//!
//! A full status ([`gityas_git::status::get_status`]) compares every tracked file with the index and walks the whole
//! tree for untracked files (~180 ms for 4 400 files on Windows). Saving a file changes the status of that file
//! only, and the watcher reports which paths changed, so [`load`] re-checks just those paths
//! ([`gityas_git::status::get_status_paths`]) and patches the previous status. Only index→worktree entries change
//! this way: the staged side depends on the index and HEAD, and any change to those is a full scan.
//!
//! A full scan instead when the patch could be wrong or is not worth it:
//! * no previous status for this session, or the last full scan is older than [`FULL_SCAN_EVERY`] (a safety net
//!   for events the watcher could have missed);
//! * the watcher batch did not carry paths (`.git/index`, HEAD, `.gitignore`, a watcher error, …) or carried more
//!   than [`MAX_INCREMENTAL_PATHS`];
//! * a changed path sits under a Windows directory link (that status needs the whole-tree pass);
//! * rename pairing could change: the patch touches a deleted or untracked file while the other kind exists, or a
//!   changed path is part of a previously paired rename.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use gityas_core::Result;
use gityas_core::models::{Delta, DeltaChange, FileState};

use crate::Inner;
use crate::session::SessionCtx;

/// A full scan at least this often, whatever the watcher says.
pub(crate) const FULL_SCAN_EVERY: Duration = Duration::from_secs(120);

/// Batches with more changed paths than this are rescanned whole (a checkout or build touching many files).
pub(crate) const MAX_INCREMENTAL_PATHS: usize = 256;

#[derive(Default)]
pub(crate) struct StatusCache(Mutex<Option<Entry>>);

struct Entry {
    /// Session generation the status belongs to.
    generation: u64,
    state: FileState,
    full_scan_at: Instant,
}

impl StatusCache {
    fn lock(&self) -> MutexGuard<'_, Option<Entry>> {
        // Guards plain data only; a poisoned lock still holds a structurally valid value.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// How the last [`load`] produced its status (tests, logs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scan {
    Full,
    Incremental,
}

/// The working-tree status of the session's repository. `changed`: repository-relative paths that are known to be
/// the only working-tree changes since the previous call (from the file watcher), or `None` when unknown.
pub(crate) fn load(inner: &Inner, ctx: &SessionCtx, repo: &git2::Repository, changed: Option<&[PathBuf]>) -> Result<FileState> {
    load_with_scan(inner, ctx, repo, changed).map(|(state, _)| state)
}

pub(crate) fn load_with_scan(
    inner: &Inner,
    ctx: &SessionCtx,
    repo: &git2::Repository,
    changed: Option<&[PathBuf]>,
) -> Result<(FileState, Scan)> {
    let started = Instant::now();
    if let Some(paths) = changed
        && let Some(state) = patch(inner, ctx, repo, paths)?
    {
        tracing::debug!(repo = %ctx.repo_id, paths = paths.len(), elapsed_ms = started.elapsed().as_millis(), "status patched");
        return Ok((state, Scan::Incremental));
    }
    let state = gityas_git::status::get_status(repo)?;
    *inner.worktree_status.lock() = Some(Entry {
        generation: ctx.generation,
        state: state.clone(),
        full_scan_at: started,
    });
    tracing::debug!(repo = %ctx.repo_id, elapsed_ms = started.elapsed().as_millis(), "status scanned");
    Ok((state, Scan::Full))
}

/// The previous status with `paths` re-checked, or `None` when a full scan is needed (see the module docs).
fn patch(inner: &Inner, ctx: &SessionCtx, repo: &git2::Repository, paths: &[PathBuf]) -> Result<Option<FileState>> {
    if paths.is_empty() || paths.len() > MAX_INCREMENTAL_PATHS {
        return Ok(None);
    }
    let Some(workdir) = repo.workdir() else { return Ok(None) };
    let previous = {
        let cache = inner.worktree_status.lock();
        match cache.as_ref() {
            Some(entry) if entry.generation == ctx.generation && entry.full_scan_at.elapsed() < FULL_SCAN_EVERY => entry.state.clone(),
            _ => return Ok(None),
        }
    };
    if paths.iter().any(|p| gityas_git::status::under_linked_dir(workdir, p)) {
        return Ok(None);
    }
    let touches = |d: &Delta| {
        paths
            .iter()
            .any(|p| covers(p, &d.path) || d.old_path.as_ref().is_some_and(|o| covers(p, o)))
    };
    if previous
        .unstaged
        .iter()
        .any(|d| matches!(d.change, DeltaChange::Renamed | DeltaChange::Copied) && touches(d))
    {
        return Ok(None);
    }
    let fresh = gityas_git::status::get_status_paths(repo, paths)?;
    let mut unstaged: Vec<Delta> = previous.unstaged.into_iter().filter(|d| !touches(d)).collect();
    let pairable = |d: &Delta| matches!(d.change, DeltaChange::Deleted | DeltaChange::Untracked);
    if fresh.iter().any(pairable) {
        let deleted = unstaged.iter().chain(&fresh).any(|d| d.change == DeltaChange::Deleted);
        let untracked = unstaged.iter().chain(&fresh).any(|d| d.change == DeltaChange::Untracked);
        if deleted && untracked {
            return Ok(None); // a rename could pair them: let the full scan decide
        }
    }
    unstaged.extend(fresh);
    gityas_git::status::sort_deltas(&mut unstaged);
    unstaged.dedup();
    let state = FileState {
        staged: previous.staged,
        unstaged,
    };
    if let Some(entry) = inner.worktree_status.lock().as_mut()
        && entry.generation == ctx.generation
    {
        entry.state = state.clone();
    }
    Ok(Some(state))
}

/// `spec` (a changed path) covers `path` (a status entry): the same path, or `path` lies under the directory `spec`.
/// Case-insensitive on Windows and macOS, whose file systems usually are.
fn covers(spec: &Path, path: &Path) -> bool {
    let norm = |p: &Path| {
        let text = p.to_string_lossy().replace('\\', "/");
        let text = text.trim_end_matches('/').to_owned();
        if cfg!(any(windows, target_os = "macos")) {
            text.to_lowercase()
        } else {
            text
        }
    };
    let (spec, path) = (norm(spec), norm(path));
    path == spec || path.strip_prefix(&spec).is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::test_engine;

    struct Fixture {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("{e}"));
            let path = dir.path().to_path_buf();
            let git = |args: &[&str]| {
                let ok = std::process::Command::new("git")
                    .args(["-c", "user.name=T", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
                    .args(args)
                    .current_dir(&path)
                    .status()
                    .is_ok_and(|s| s.success());
                assert!(ok, "git {args:?}");
            };
            git(&["init", "-q", "-b", "main"]);
            std::fs::create_dir_all(path.join("dir")).unwrap_or_else(|e| panic!("{e}"));
            for (file, content) in [
                ("a.txt", "a\n"),
                ("b.txt", "b\n"),
                ("dir/c.txt", "c\n"),
                ("old.txt", &"line\n".repeat(30)),
            ] {
                std::fs::write(path.join(file), content).unwrap_or_else(|e| panic!("{e}"));
            }
            git(&["add", "-A"]);
            git(&["commit", "-qm", "base"]);
            Self { _dir: dir, path }
        }

        fn write(&self, rel: &str, content: &str) {
            let full = self.path.join(rel);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap_or_else(|e| panic!("{e}"));
            }
            std::fs::write(full, content).unwrap_or_else(|e| panic!("{e}"));
        }
    }

    /// Loads with `changed`, asserts the scan kind, and that the result equals a fresh full status.
    fn step(t: &crate::testutil::TestEngine, ctx: &SessionCtx, fx: &Fixture, changed: Option<&[&str]>, expect: Scan) {
        let repo = git2::Repository::open(&fx.path).unwrap_or_else(|e| panic!("{e}"));
        let changed: Option<Vec<PathBuf>> = changed.map(|c| c.iter().map(PathBuf::from).collect());
        let (state, scan) = load_with_scan(&t.engine.inner, ctx, &repo, changed.as_deref()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(scan, expect, "{changed:?}");
        let full = gityas_git::status::get_status(&repo).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(state, full, "{changed:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn edits_patch_the_status_and_risky_changes_rescan() {
        let t = test_engine();
        let fx = Fixture::new();
        let ctx = t.install_session(&fx.path);
        step(&t, &ctx, &fx, Some(&["a.txt"]), Scan::Full); // nothing known yet
        step(&t, &ctx, &fx, None, Scan::Full);

        fx.write("a.txt", "a changed\n");
        step(&t, &ctx, &fx, Some(&["a.txt"]), Scan::Incremental);
        fx.write("a.txt", "a\n"); // back to the committed content: no longer modified
        step(&t, &ctx, &fx, Some(&["a.txt"]), Scan::Incremental);
        fx.write("dir/new.txt", "new\n");
        fx.write("dir/c.txt", "c changed\n");
        step(&t, &ctx, &fx, Some(&["dir"]), Scan::Incremental); // a directory covers its contents
        fx.write("fresh/deep/x.txt", "x\n");
        step(&t, &ctx, &fx, Some(&["fresh"]), Scan::Incremental);

        // Deleting a tracked file while untracked files exist: they could pair as a rename.
        std::fs::remove_file(fx.path.join("old.txt")).unwrap_or_else(|e| panic!("{e}"));
        step(&t, &ctx, &fx, Some(&["old.txt"]), Scan::Full);
        // A rename the full scan paired, touched again: rescan.
        fx.write("renamed.txt", &"line\n".repeat(30));
        step(&t, &ctx, &fx, Some(&["renamed.txt"]), Scan::Full);
        step(&t, &ctx, &fx, Some(&["renamed.txt"]), Scan::Full);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn another_session_or_too_many_paths_rescan() {
        let t = test_engine();
        let fx = Fixture::new();
        let ctx = t.install_session(&fx.path);
        step(&t, &ctx, &fx, None, Scan::Full);
        fx.write("b.txt", "b changed\n");
        let many: Vec<String> = (0..=MAX_INCREMENTAL_PATHS).map(|i| format!("p{i}.txt")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).chain(["b.txt"]).collect();
        step(&t, &ctx, &fx, Some(&many), Scan::Full);
        // A new session (generation) of the same repository.
        let other = t.engine.inner.sessions.new_ctx(ctx.repo_id.clone(), fx.path.clone(), None);
        fx.write("b.txt", "b changed again\n");
        step(&t, &other, &fx, Some(&["b.txt"]), Scan::Full);
        step(&t, &other, &fx, Some(&["b.txt"]), Scan::Incremental);
    }

    #[test]
    fn covering_paths() {
        assert!(covers(Path::new("a/b.txt"), Path::new("a/b.txt")));
        assert!(covers(Path::new("a"), Path::new("a/b.txt")));
        assert!(covers(Path::new("a"), Path::new("a/")));
        assert!(!covers(Path::new("a"), Path::new("ab.txt")));
        assert!(!covers(Path::new("a/b.txt"), Path::new("a")));
        #[cfg(windows)]
        assert!(covers(Path::new("A\\B.txt"), Path::new("a/b.txt")));
    }
}
