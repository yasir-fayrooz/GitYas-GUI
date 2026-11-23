//! The session's repository watcher: one `notify-debouncer-mini` debouncer over the repository's roots, a path
//! classifier, and a task that turns each debounced batch into one coalesced refresh.
//!
//! **Why one watcher that classifies, not four watchers.** The old app ran four watchers (files, HEAD, refs/heads,
//! refs/remotes), each with its own debounce, so one commit fired up to four independent refreshes and the
//! `refs/remotes` watcher failed outright when that directory did not exist yet. Here a single watcher sees every
//! change, [`Layout::classify`] maps each path to a [`WatchKind`] (or noise), and the union of the batch's kinds is
//! refreshed once.
//!
//! **Roots.** The working tree (recursive; absent for a bare repository), the per-worktree git dir (`HEAD`, `index`,
//! merge state) and the common git dir (`refs`, `packed-refs`, `config`), each canonicalised, minus roots already
//! inside another root. Normal repository: just the working tree (`.git` is inside it). Linked worktree: its working
//! tree plus the main repository's common dir (which contains this worktree's git dir under `worktrees/<name>`).
//! Bare: the git dir.
//!
//! **Noise ignored:** inside a git dir, `objects/**`, `logs/**`, `*.lock`, `hooks/**`, `modules/**` (submodule git
//! dirs), other worktrees' state (`worktrees/**`), `FETCH_HEAD`/`ORIG_HEAD`/`COMMIT_EDITMSG` and anything unknown;
//! in the working tree, any path with a `.git` component (a submodule's or nested repository's git dir) and
//! gitignored paths (`Repository::is_path_ignored`, cached per parent directory so a `node_modules` storm costs one
//! ignore lookup per directory). `*.lock` is only noise inside git dirs: `Cargo.lock` in the working tree is a file.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gityas_core::{AppEvent, GitYasError, Result};
use notify_debouncer_mini::notify::{self, PollWatcher, RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{Config, DebounceEventResult, Debouncer, new_debouncer_opt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::Inner;
use crate::refresh::{Refresh, refresh_blocking};
use crate::session::SessionCtx;

/// What kind of repository state a changed path affects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum WatchKind {
    /// Working tree, `index`, `info/exclude`.
    Files,
    /// `HEAD` and in-progress operation state (`MERGE_HEAD`, `rebase-merge/`, …).
    Head,
    /// `refs/heads`, `refs/tags`, other local refs (stash, notes).
    RefsHeads,
    /// `refs/remotes`, and the remote configuration (`config`).
    RefsRemotes,
}

impl WatchKind {
    pub(crate) const fn refresh(self) -> Refresh {
        match self {
            WatchKind::Files => Refresh::FILES,
            WatchKind::Head => Refresh::HEAD,
            WatchKind::RefsHeads => Refresh::LOCAL_REFS,
            WatchKind::RefsRemotes => Refresh::REMOTE_REFS,
        }
    }
}

/// Which notify backend to use. `Native` is FSEvents / inotify / ReadDirectoryChangesW; `Poll` is notify's portable
/// polling watcher (tests and environments without native events).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    Native,
    #[cfg_attr(not(test), allow(dead_code))]
    Poll(Duration),
}

/// Keeps the debouncer (and its thread) alive; dropping it stops watching.
enum Guard {
    Native(#[allow(dead_code)] Debouncer<RecommendedWatcher>),
    Poll(#[allow(dead_code)] Debouncer<PollWatcher>),
}

/// The repository's directories, canonicalised (FSEvents reports real paths, e.g. `/private/var/…` on macOS).
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    /// `None` for a bare repository.
    workdir: Option<PathBuf>,
    /// Per-worktree git dir (`.git`, or `<common>/worktrees/<name>` for a linked worktree).
    gitdir: PathBuf,
    /// Shared git dir (refs, objects, config). Equal to `gitdir` except in a linked worktree.
    commondir: PathBuf,
    /// Directories to watch recursively: no root lies inside another.
    roots: Vec<PathBuf>,
}

fn canonical(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).map_err(|e| GitYasError::Io(format!("{}: {e}", path.display())))
}

impl Layout {
    pub(crate) fn of(repo: &git2::Repository) -> Result<Layout> {
        let gitdir = canonical(repo.path())?;
        let commondir = canonical(repo.commondir())?;
        let workdir = repo.workdir().map(canonical).transpose()?;
        let mut candidates: Vec<PathBuf> = workdir.iter().cloned().collect();
        candidates.push(gitdir.clone());
        candidates.push(commondir.clone());
        let mut roots: Vec<PathBuf> = Vec::new();
        for candidate in &candidates {
            let covered = candidates.iter().any(|other| other != candidate && candidate.starts_with(other));
            if !covered && !roots.contains(candidate) {
                roots.push(candidate.clone());
            }
        }
        Ok(Layout {
            workdir,
            gitdir,
            commondir,
            roots,
        })
    }

    pub(crate) fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// `false` once the repository (its git dir or working tree) has been deleted or moved away.
    pub(crate) fn exists(&self) -> bool {
        self.gitdir.is_dir() && self.commondir.is_dir() && self.workdir.as_ref().is_none_or(|w| w.is_dir())
    }

    /// The kind of state `path` affects, or `None` for noise.
    pub(crate) fn classify(&self, path: &Path, ignore: &mut IgnoreCheck) -> Option<WatchKind> {
        // The per-worktree git dir first: in a linked worktree it lies inside the common dir.
        if let Ok(rel) = path.strip_prefix(&self.gitdir) {
            return classify_git(rel, true, self.gitdir == self.commondir);
        }
        if let Ok(rel) = path.strip_prefix(&self.commondir) {
            return classify_git(rel, false, true);
        }
        let rel = path.strip_prefix(self.workdir.as_ref()?).ok()?;
        if rel.as_os_str().is_empty() || rel.components().any(|c| c.as_os_str() == ".git") {
            return None;
        }
        (!ignore.is_ignored(rel)).then_some(WatchKind::Files)
    }
}

/// Classifies a path relative to a git dir. `per_worktree`: it is this worktree's git dir (`HEAD`, `index`, merge
/// state count); `shared`: it is the common dir (refs, `packed-refs`, `config` count).
fn classify_git(rel: &Path, per_worktree: bool, shared: bool) -> Option<WatchKind> {
    if rel.extension() == Some(OsStr::new("lock")) {
        return None;
    }
    let mut parts = rel.components().filter_map(|c| match c {
        Component::Normal(p) => Some(p),
        _ => None,
    });
    let first = parts.next()?;
    let second = parts.next();
    let single = second.is_none();
    let is = |name: &str| first == OsStr::new(name);
    if is("refs") {
        // Per-worktree refs (`refs/bisect`, `refs/worktree`) live in the worktree git dir; treat them as local refs.
        return match second {
            None => None,
            Some(s) if s == OsStr::new("remotes") => shared.then_some(WatchKind::RefsRemotes),
            Some(_) => Some(WatchKind::RefsHeads),
        };
    }
    if per_worktree {
        if single && is("HEAD") {
            return Some(WatchKind::Head);
        }
        if single && is("index") {
            return Some(WatchKind::Files);
        }
        let op_state = [
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "REBASE_HEAD",
            "rebase-merge",
            "rebase-apply",
        ];
        if op_state.iter().any(|n| is(n)) {
            return Some(WatchKind::Head);
        }
    }
    if shared {
        if single && is("packed-refs") {
            // Packed refs hold local and remote-tracking refs alike; the caller adds both (see `refresh_for`).
            return Some(WatchKind::RefsHeads);
        }
        if single && is("config") {
            return Some(WatchKind::RefsRemotes);
        }
        if is("info") && second == Some(OsStr::new("exclude")) {
            return Some(WatchKind::Files);
        }
    }
    None
}

/// `Refresh` for one changed path (kinds plus the `packed-refs` special case).
fn refresh_for(path: &Path, kind: WatchKind) -> Refresh {
    let refresh = kind.refresh();
    if path.file_name() == Some(OsStr::new("packed-refs")) && kind == WatchKind::RefsHeads {
        refresh.union(Refresh::REMOTE_REFS)
    } else {
        refresh
    }
}

/// Gitignore lookups for one batch. Caches "is this directory ignored" per parent directory, so thousands of
/// changes under `node_modules/` or `target/` cost one lookup per directory. Without a repository (it could not be
/// opened) nothing counts as ignored: refreshing too often beats missing a change.
pub(crate) struct IgnoreCheck {
    repo: Option<git2::Repository>,
    dirs: HashMap<PathBuf, bool>,
}

impl IgnoreCheck {
    pub(crate) fn new(repo: Option<git2::Repository>) -> Self {
        Self {
            repo,
            dirs: HashMap::new(),
        }
    }

    fn lookup(&self, path: &Path) -> bool {
        let Some(repo) = &self.repo else { return false };
        repo.is_path_ignored(path).unwrap_or_else(|e| {
            tracing::debug!(path = %path.display(), "ignore lookup failed: {e}");
            false
        })
    }

    fn dir_ignored(&mut self, dir: &Path) -> bool {
        if let Some(&ignored) = self.dirs.get(dir) {
            return ignored;
        }
        let ignored = match dir.parent() {
            Some(parent) if !parent.as_os_str().is_empty() && self.dir_ignored(parent) => true,
            _ => {
                // A trailing slash makes `dir/` patterns match even if the directory was just deleted.
                let mut with_slash = OsString::from(dir.as_os_str());
                with_slash.push("/");
                self.lookup(Path::new(&with_slash))
            }
        };
        self.dirs.insert(dir.to_path_buf(), ignored);
        ignored
    }

    /// `rel`: path relative to the working tree.
    pub(crate) fn is_ignored(&mut self, rel: &Path) -> bool {
        match rel.parent() {
            Some(parent) if !parent.as_os_str().is_empty() && self.dir_ignored(parent) => true,
            _ => self.lookup(rel),
        }
    }
}

/// A debounced batch: changed paths, or "the watcher reported an error" (state unknown, refresh everything).
#[derive(Debug, Default)]
struct Batch {
    paths: Vec<PathBuf>,
    errors: usize,
}

impl Batch {
    fn absorb(&mut self, result: DebounceEventResult) {
        match result {
            Ok(events) => self.paths.extend(events.into_iter().map(|e| e.path)),
            Err(err) => {
                tracing::warn!("file watcher error: {err}");
                self.errors += 1;
            }
        }
    }
}

/// Outcome of classifying a batch.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Refresh(Refresh),
    /// The repository directory is gone: warn once and stop watching.
    Gone,
}

fn judge(layout: &Layout, batch: &Batch, repo_path: &Path) -> Verdict {
    if !layout.exists() {
        return Verdict::Gone;
    }
    if batch.errors > 0 {
        // Lost events (queue overflow, rescan) or a backend error: the state is unknown.
        return Verdict::Refresh(Refresh::ALL);
    }
    let mut ignore = IgnoreCheck::new(
        gityas_git::repository::open(repo_path)
            .inspect_err(|e| tracing::debug!("watcher: cannot open repository for ignore rules: {e}"))
            .ok(),
    );
    let mut what = Refresh::NONE;
    for path in &batch.paths {
        if let Some(kind) = layout.classify(path, &mut ignore) {
            what = what.union(refresh_for(path, kind));
            if what == Refresh::ALL {
                break;
            }
        }
    }
    if layout.workdir.is_none() {
        what.files = false;
    }
    Verdict::Refresh(what)
}

fn start_debouncer(layout: &Layout, debounce: Duration, backend: Backend, tx: mpsc::UnboundedSender<DebounceEventResult>) -> Result<Guard> {
    let handler = move |result: DebounceEventResult| {
        // The receiver is gone only when the watcher task has ended; nothing left to notify.
        if tx.send(result).is_err() {
            tracing::trace!("watcher task ended; dropping file event");
        }
    };
    let watch_err = |e: notify::Error| GitYasError::Io(format!("cannot watch the repository: {e}"));
    let config = Config::default().with_timeout(debounce);
    let mut guard = match backend {
        Backend::Native => Guard::Native(new_debouncer_opt::<_, RecommendedWatcher>(config, handler).map_err(watch_err)?),
        Backend::Poll(every) => Guard::Poll(
            new_debouncer_opt::<_, PollWatcher>(
                config.with_notify_config(notify::Config::default().with_poll_interval(every)),
                handler,
            )
            .map_err(watch_err)?,
        ),
    };
    for root in layout.roots() {
        let watcher = match &mut guard {
            Guard::Native(d) => d.watcher(),
            Guard::Poll(d) => d.watcher(),
        };
        watcher.watch(root, RecursiveMode::Recursive).map_err(watch_err)?;
    }
    Ok(guard)
}

/// Upper bound on the extra coalescing wait after the first batch, in debounce windows.
const SETTLE_CAP: u32 = 3;

/// Spawns the session's watcher task on `inner.runtime`.
pub(crate) fn spawn(inner: &Arc<Inner>, ctx: &SessionCtx, backend: Backend) -> JoinHandle<()> {
    let inner = Arc::clone(inner);
    let ctx = ctx.clone();
    inner.runtime.clone().spawn(async move { run(inner, ctx, backend).await })
}

async fn run(inner: Arc<Inner>, ctx: SessionCtx, backend: Backend) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let debounce = inner.options.watch_debounce;
    let path = ctx.path.clone();
    let started = inner
        .blocking(move || {
            let repo = gityas_git::repository::open(&path)?;
            let layout = Layout::of(&repo)?;
            let guard = start_debouncer(&layout, debounce, backend, tx)?;
            Ok((layout, guard))
        })
        .await;
    let (layout, _guard) = match started {
        Ok(started) => started,
        Err(err) => {
            tracing::warn!(repo = %ctx.repo_id, "file watcher not started: {err}");
            inner.emit_scoped(
                &ctx,
                AppEvent::Warning(format!("Live updates are off for this repository ({err}). Use Refresh to update.")),
            );
            return;
        }
    };
    tracing::debug!(repo = %ctx.repo_id, roots = ?layout.roots(), "file watcher started");
    let layout = Arc::new(layout);

    loop {
        let first = tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => break,
            next = rx.recv() => match next {
                Some(result) => result,
                None => {
                    tracing::warn!(repo = %ctx.repo_id, "file watcher stopped unexpectedly");
                    break;
                }
            },
        };
        // Coalesce: wait until the burst settles (no new batch for one debounce window, capped at
        // `SETTLE_CAP` windows) so a long git operation that spans several debounce windows is one refresh.
        let mut batch = Batch::default();
        batch.absorb(first);
        let cap = tokio::time::Instant::now() + debounce * SETTLE_CAP;
        loop {
            let window = tokio::time::Instant::now() + debounce;
            tokio::select! {
                biased;
                () = ctx.cancel.cancelled() => return,
                more = rx.recv() => match more {
                    Some(more) => batch.absorb(more),
                    None => break,
                },
                () = tokio::time::sleep_until(window.min(cap)) => break,
            }
            if tokio::time::Instant::now() >= cap {
                break;
            }
        }

        let judged = {
            let layout = Arc::clone(&layout);
            let path = ctx.path.clone();
            inner.blocking(move || Ok(judge(&layout, &batch, &path))).await
        };
        let what = match judged {
            Ok(Verdict::Refresh(what)) => what,
            Ok(Verdict::Gone) => {
                tracing::warn!(repo = %ctx.repo_id, "repository directory disappeared; file watcher stopped");
                inner.emit_scoped(
                    &ctx,
                    AppEvent::Warning(format!(
                        "The repository at {} no longer exists; live updates stopped.",
                        ctx.path.display()
                    )),
                );
                break;
            }
            Err(GitYasError::Cancelled) => break,
            Err(err) => {
                tracing::warn!(repo = %ctx.repo_id, "watcher: classifying changes failed: {err}");
                continue;
            }
        };
        if what.is_empty() {
            continue;
        }
        tracing::trace!(repo = %ctx.repo_id, ?what, "watcher refresh");
        match inner
            .with_session_repo(&ctx, move |inner, ctx, repo| refresh_blocking(inner, ctx, repo, what))
            .await
        {
            Ok(()) => {}
            Err(GitYasError::Cancelled) => break,
            // Parts that succeeded were emitted; a transient failure (e.g. mid-rebase) is retried on the next change.
            Err(err) => tracing::warn!(repo = %ctx.repo_id, "watcher refresh failed: {err}"),
        }
    }
    tracing::debug!(repo = %ctx.repo_id, "file watcher stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(workdir: Option<&str>, gitdir: &str, commondir: &str) -> Layout {
        let workdir = workdir.map(PathBuf::from);
        let mut l = Layout {
            workdir: workdir.clone(),
            gitdir: gitdir.into(),
            commondir: commondir.into(),
            roots: vec![],
        };
        l.roots = workdir.into_iter().chain([l.commondir.clone()]).collect();
        l
    }

    fn kind(l: &Layout, path: &str) -> Option<WatchKind> {
        l.classify(Path::new(path), &mut IgnoreCheck::new(None))
    }

    #[test]
    fn classifies_a_normal_repository() {
        let l = layout(Some("/w"), "/w/.git", "/w/.git");
        assert_eq!(kind(&l, "/w/src/main.rs"), Some(WatchKind::Files));
        assert_eq!(
            kind(&l, "/w/Cargo.lock"),
            Some(WatchKind::Files),
            "*.lock is only noise inside .git"
        );
        assert_eq!(kind(&l, "/w/.git/index"), Some(WatchKind::Files));
        assert_eq!(kind(&l, "/w/.git/HEAD"), Some(WatchKind::Head));
        assert_eq!(kind(&l, "/w/.git/MERGE_HEAD"), Some(WatchKind::Head));
        assert_eq!(kind(&l, "/w/.git/rebase-merge/done"), Some(WatchKind::Head));
        assert_eq!(kind(&l, "/w/.git/refs/heads/main"), Some(WatchKind::RefsHeads));
        assert_eq!(kind(&l, "/w/.git/refs/heads/feature/x"), Some(WatchKind::RefsHeads));
        assert_eq!(kind(&l, "/w/.git/refs/tags/v1"), Some(WatchKind::RefsHeads));
        assert_eq!(kind(&l, "/w/.git/refs/remotes/origin/main"), Some(WatchKind::RefsRemotes));
        assert_eq!(kind(&l, "/w/.git/packed-refs"), Some(WatchKind::RefsHeads));
        assert_eq!(kind(&l, "/w/.git/config"), Some(WatchKind::RefsRemotes));
        assert_eq!(kind(&l, "/w/.git/info/exclude"), Some(WatchKind::Files));
        for noise in [
            "/w/.git/objects/ab/cdef",
            "/w/.git/logs/HEAD",
            "/w/.git/logs/refs/heads/main",
            "/w/.git/index.lock",
            "/w/.git/refs/heads/main.lock",
            "/w/.git/HEAD.lock",
            "/w/.git/FETCH_HEAD",
            "/w/.git/ORIG_HEAD",
            "/w/.git/COMMIT_EDITMSG",
            "/w/.git/hooks/pre-commit",
            "/w/.git/modules/sub/HEAD",
            "/w/.git/worktrees/other/HEAD",
            "/w/.git",
            "/w/.git/refs",
            "/w",
            "/w/sub/.git",
            "/w/vendor/nested/.git/HEAD",
            "/elsewhere/file",
        ] {
            assert_eq!(kind(&l, noise), None, "{noise}");
        }
    }

    #[test]
    fn packed_refs_refresh_local_and_remote_refs() {
        let r = refresh_for(Path::new("/w/.git/packed-refs"), WatchKind::RefsHeads);
        assert_eq!(r, Refresh::LOCAL_REFS.union(Refresh::REMOTE_REFS));
    }

    #[test]
    fn classifies_a_linked_worktree() {
        // Worktree at /wt, main repository at /main.
        let l = layout(Some("/wt"), "/main/.git/worktrees/wt", "/main/.git");
        assert_eq!(kind(&l, "/wt/file"), Some(WatchKind::Files));
        assert_eq!(kind(&l, "/main/.git/worktrees/wt/HEAD"), Some(WatchKind::Head));
        assert_eq!(kind(&l, "/main/.git/worktrees/wt/index"), Some(WatchKind::Files));
        assert_eq!(kind(&l, "/main/.git/refs/heads/b"), Some(WatchKind::RefsHeads));
        assert_eq!(kind(&l, "/main/.git/refs/remotes/o/b"), Some(WatchKind::RefsRemotes));
        assert_eq!(kind(&l, "/main/.git/packed-refs"), Some(WatchKind::RefsHeads));
        // The main worktree's own HEAD / index are not this session's.
        assert_eq!(kind(&l, "/main/.git/HEAD"), None);
        assert_eq!(kind(&l, "/main/.git/index"), None);
        assert_eq!(kind(&l, "/main/.git/worktrees/other/HEAD"), None);
        // Refs are shared, so they are not read from the worktree git dir's `config`/`packed-refs`.
        assert_eq!(kind(&l, "/main/.git/worktrees/wt/packed-refs"), None);
        // The main working tree is not watched.
        assert_eq!(kind(&l, "/main/file"), None);
    }

    #[test]
    fn classifies_a_bare_repository() {
        let l = layout(None, "/r.git", "/r.git");
        assert_eq!(kind(&l, "/r.git/HEAD"), Some(WatchKind::Head));
        assert_eq!(kind(&l, "/r.git/refs/heads/main"), Some(WatchKind::RefsHeads));
        assert_eq!(kind(&l, "/r.git/objects/pack/x.pack"), None);
        assert_eq!(kind(&l, "/elsewhere"), None);
    }

    #[test]
    fn roots_are_minimal() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let l = Layout::of(&repo).unwrap();
        assert_eq!(l.roots(), [std::fs::canonicalize(dir.path()).unwrap()]);

        let bare_dir = tempfile::tempdir().unwrap();
        let bare = git2::Repository::init_bare(bare_dir.path()).unwrap();
        let l = Layout::of(&bare).unwrap();
        assert_eq!(l.roots(), [std::fs::canonicalize(bare_dir.path()).unwrap()]);
        assert!(l.workdir.is_none());
    }
}
