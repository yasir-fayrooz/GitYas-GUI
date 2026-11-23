//! Active-repository session plumbing shared by every command module.
//!
//! * **Command lock.** Every command that touches the active repository, and every session switch, runs under
//!   [`SessionState::command_lock`]. A command therefore always sees one consistent session and cannot race a tab switch.
//! * **Switching never waits for the network.** Before taking the lock, `open_repository` / `close_repository` call
//!   [`SessionState::cancel_active`], which cancels the session token *and* aborts the session's in-flight remote
//!   operations (their `RemoteContext` cancel flags), so a long fetch/push holding the lock fails fast with
//!   `GitYasError::Cancelled` and the switch proceeds.
//! * **Stale events are dropped.** Repo-scoped events go through [`Inner::emit_scoped`], which drops them once the
//!   session they were computed for is cancelled or replaced. The frontend's `repo` filter is the second guard.
//! * **Background tasks** (watchers, fetch scheduler) hold an `Arc<Inner>` and a [`SessionCtx`] clone, stop when
//!   `ctx.cancel` fires, and never take the command lock for long (the scheduler's fetch takes it like any command).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use gityas_core::{AppEvent, GitYasError, Provider, RepoId, Result};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{Engine, Inner};

/// Immutable snapshot of the active session, cheap to clone into tasks and blocking closures.
#[derive(Debug, Clone)]
pub(crate) struct SessionCtx {
    pub(crate) repo_id: RepoId,
    /// Working directory (or `.git` dir for a bare repo) as registered; open with `gityas_git::repository::open`.
    pub(crate) path: PathBuf,
    /// The repository's stored provider, passed to `RemoteContext::with_provider`.
    pub(crate) provider: Option<Provider>,
    /// Strictly increasing per session; never reused within a process.
    pub(crate) generation: u64,
    /// Cancelled when the session ends (switch, close, delete, shutdown).
    pub(crate) cancel: CancellationToken,
}

impl SessionCtx {
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

/// The active session: its snapshot plus the background tasks it owns (watchers, scheduler).
pub(crate) struct Session {
    pub(crate) ctx: SessionCtx,
    pub(crate) tasks: Vec<JoinHandle<()>>,
}

impl Session {
    /// Cancels the token and waits for every task to finish. A task that panicked is logged, not propagated.
    pub(crate) async fn stop(self) {
        self.ctx.cancel.cancel();
        for task in self.tasks {
            if let Err(err) = task.await
                && err.is_panic()
            {
                tracing::error!(repo = %self.ctx.repo_id, "session task panicked: {err}");
            }
        }
    }
}

/// One in-flight remote operation (fetch / pull / push / clone / tag push / connection test).
struct RemoteOpEntry {
    flag: Arc<AtomicBool>,
    /// Session generation the operation belongs to; `None` for clone (not tied to a session).
    generation: Option<u64>,
}

#[derive(Default)]
pub(crate) struct SessionState {
    /// Serialises commands and session switches. Hold it for the whole command, including the blocking work.
    pub(crate) command_lock: tokio::sync::Mutex<()>,
    active: Mutex<Option<Session>>,
    generation: AtomicU64,
    remote_ops: Mutex<Vec<RemoteOpEntry>>,
    /// Set by `Engine::shutdown`; no new session may be installed afterwards.
    shut_down: AtomicBool,
}

/// A std mutex here only guards plain data and is never held across `.await`; a poisoned lock just means another
/// thread panicked mid-update of a value that is still structurally valid.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl SessionState {
    /// Snapshot of the active session, if any. Does not take the command lock.
    pub(crate) fn active(&self) -> Option<SessionCtx> {
        lock(&self.active).as_ref().map(|s| s.ctx.clone())
    }

    /// Marks the engine as shut down (see [`SessionState::is_shut_down`]). Irreversible.
    pub(crate) fn mark_shut_down(&self) {
        self.shut_down.store(true, Ordering::SeqCst);
    }

    /// `true` once `Engine::shutdown` has started. Session-installing code checks it with the command lock held, so a
    /// switch that was waiting for the lock when shutdown ran cannot start a session (watchers, scheduler) after it.
    pub(crate) fn is_shut_down(&self) -> bool {
        self.shut_down.load(Ordering::SeqCst)
    }

    /// The active session or `GitYasError::NoActiveRepository`.
    pub(crate) fn require(&self) -> Result<SessionCtx> {
        self.active().ok_or(GitYasError::NoActiveRepository)
    }

    /// `true` while `ctx` is still the active, uncancelled session.
    pub(crate) fn is_current(&self, ctx: &SessionCtx) -> bool {
        !ctx.is_cancelled() && lock(&self.active).as_ref().is_some_and(|s| s.ctx.generation == ctx.generation)
    }

    /// A fresh context for `repo_id`; does not install it (see [`SessionState::replace`]).
    pub(crate) fn new_ctx(&self, repo_id: RepoId, path: PathBuf, provider: Option<Provider>) -> SessionCtx {
        SessionCtx {
            repo_id,
            path,
            provider,
            generation: self.generation.fetch_add(1, Ordering::SeqCst) + 1,
            cancel: CancellationToken::new(),
        }
    }

    /// Installs `new` (or clears with `None`) and returns the previous session, which the caller must
    /// [`Session::stop`]. Call with the command lock held.
    pub(crate) fn replace(&self, new: Option<Session>) -> Option<Session> {
        std::mem::replace(&mut *lock(&self.active), new)
    }

    /// Updates the provider of the active session if it is `repo_id`'s (after `set_repository_provider`), so later
    /// commands (`with_repo` reads a fresh snapshot) authenticate with it. Tasks holding an older `SessionCtx`
    /// clone keep the old value; they should take the provider from `with_repo`'s ctx. Returns whether it changed.
    pub(crate) fn set_active_provider(&self, repo_id: &str, provider: Option<Provider>) -> bool {
        match lock(&self.active).as_mut() {
            Some(session) if session.ctx.repo_id == repo_id => {
                session.ctx.provider = provider;
                true
            }
            _ => false,
        }
    }

    /// Attaches a task to the active session if it is still `ctx`'s; otherwise aborts it.
    pub(crate) fn attach_task(&self, ctx: &SessionCtx, task: JoinHandle<()>) {
        let mut active = lock(&self.active);
        match active.as_mut() {
            Some(session) if session.ctx.generation == ctx.generation => session.tasks.push(task),
            _ => task.abort(),
        }
    }

    /// Cancels the active session's token and its in-flight remote operations, without taking the command lock.
    /// The session stays installed until the holder of the command lock replaces it.
    pub(crate) fn cancel_active(&self) {
        let generation = {
            let active = lock(&self.active);
            let Some(session) = active.as_ref() else { return };
            session.ctx.cancel.cancel();
            session.ctx.generation
        };
        for op in lock(&self.remote_ops).iter() {
            if op.generation == Some(generation) {
                op.flag.store(true, Ordering::SeqCst);
            }
        }
    }

    /// Registers a remote operation. Pass `&guard.flag()` to `RemoteContext::with_cancel`. `ctx: None` for clone.
    pub(crate) fn begin_remote_op(self: &Arc<Self>, ctx: Option<&SessionCtx>) -> RemoteOpGuard {
        let flag = Arc::new(AtomicBool::new(ctx.is_some_and(SessionCtx::is_cancelled)));
        lock(&self.remote_ops).push(RemoteOpEntry {
            flag: Arc::clone(&flag),
            generation: ctx.map(|c| c.generation),
        });
        RemoteOpGuard {
            state: Arc::clone(self),
            flag,
        }
    }

    /// `true` while a remote operation of session `generation` is in flight (the fetch scheduler skips its tick).
    pub(crate) fn has_remote_op(&self, generation: u64) -> bool {
        lock(&self.remote_ops).iter().any(|op| op.generation == Some(generation))
    }

    /// Aborts every in-flight remote operation (status-bar / clone-dialog Cancel). Returns how many were signalled.
    pub(crate) fn cancel_remote_ops(&self) -> usize {
        let ops = lock(&self.remote_ops);
        for op in ops.iter() {
            op.flag.store(true, Ordering::SeqCst);
        }
        ops.len()
    }
}

/// Keeps a remote operation registered for cancellation; unregisters on drop.
pub(crate) struct RemoteOpGuard {
    state: Arc<SessionState>,
    flag: Arc<AtomicBool>,
}

impl RemoteOpGuard {
    pub(crate) fn flag(&self) -> &AtomicBool {
        &self.flag
    }
}

impl Drop for RemoteOpGuard {
    fn drop(&mut self) {
        lock(&self.state.remote_ops).retain(|op| !Arc::ptr_eq(&op.flag, &self.flag));
    }
}

impl Inner {
    /// Runs blocking work (git2, SQLite, filesystem) on the blocking pool.
    pub(crate) async fn blocking<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        match self.runtime.spawn_blocking(f).await {
            Ok(result) => result,
            Err(err) if err.is_cancelled() => Err(GitYasError::Cancelled),
            Err(err) => Err(GitYasError::Other(format!("internal error: background task failed: {err}"))),
        }
    }

    /// For background tasks (watchers, scheduler), which hold an `Arc<Inner>` rather than an `Engine`: like
    /// [`Engine::with_repo`] but for the session `ctx` the task belongs to, not whatever is active. Waits for the
    /// command lock unless `ctx` is cancelled first; fails with `GitYasError::Cancelled` if `ctx` is cancelled or no
    /// longer the active session once the lock is held, so a stale task never touches the new session's repository.
    pub(crate) async fn with_session_repo<T, F>(self: &Arc<Self>, ctx: &SessionCtx, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Inner, &SessionCtx, &mut git2::Repository) -> Result<T> + Send + 'static,
    {
        let _guard = tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => return Err(GitYasError::Cancelled),
            guard = self.sessions.command_lock.lock() => guard,
        };
        if !self.sessions.is_current(ctx) {
            return Err(GitYasError::Cancelled);
        }
        let inner = Arc::clone(self);
        let ctx = ctx.clone();
        self.blocking(move || {
            let mut repo = gityas_git::repository::open(&ctx.path)?;
            f(&inner, &ctx, &mut repo)
        })
        .await
    }

    /// Emits a global (not repo-scoped) event.
    pub(crate) fn emit(&self, event: AppEvent) {
        self.events.emit(event);
    }

    /// Emits a repo-scoped event computed for `ctx`; dropped if that session is no longer current.
    pub(crate) fn emit_scoped(&self, ctx: &SessionCtx, event: AppEvent) {
        debug_assert!(event.repo().is_none_or(|r| *r == ctx.repo_id), "event for another repo");
        if self.sessions.is_current(ctx) {
            self.events.emit(event);
        } else {
            tracing::debug!(repo = %ctx.repo_id, event = event.name(), "dropped event from an inactive session");
        }
    }
}

impl Engine {
    /// Runs `f` on the blocking pool with the active session's repository freshly opened (`git2::Repository` is
    /// `!Sync`; one per command), holding the command lock throughout. `NoActiveRepository` if none is open.
    pub(crate) async fn with_repo<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Inner, &SessionCtx, &mut git2::Repository) -> Result<T> + Send + 'static,
    {
        let _guard = self.inner.sessions.command_lock.lock().await;
        let ctx = self.inner.sessions.require()?;
        let inner = Arc::clone(&self.inner);
        self.inner
            .blocking(move || {
                let mut repo = gityas_git::repository::open(&ctx.path)?;
                f(&inner, &ctx, &mut repo)
            })
            .await
    }

    /// Like [`Engine::with_repo`] but without an active session requirement: runs `f` under the command lock
    /// (repo registry and settings changes that must not interleave with a switch).
    pub(crate) async fn with_lock<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Inner) -> Result<T> + Send + 'static,
    {
        let _guard = self.inner.sessions.command_lock.lock().await;
        let inner = Arc::clone(&self.inner);
        self.inner.blocking(move || f(&inner)).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use gityas_core::{AppEvent, GitYasError};

    use crate::testutil::test_engine;

    fn warning_and_scoped(repo: &str) -> [AppEvent; 2] {
        [
            AppEvent::Warning("w".into()),
            AppEvent::RemotesUpdated {
                repo: repo.into(),
                remotes: vec![],
            },
        ]
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn commands_without_a_session_fail_with_no_active_repository() {
        let t = test_engine();
        let err = t.engine.with_repo(|_, _, _| Ok(())).await.err();
        assert_eq!(err, Some(GitYasError::NoActiveRepository));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scoped_events_from_a_replaced_session_are_dropped() {
        let t = test_engine();
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        let old = t.install_session(dir.path());
        let [w, scoped] = warning_and_scoped(&old.repo_id);
        t.engine.inner.emit_scoped(&old, scoped.clone());
        assert_eq!(t.sink.take(), vec![scoped.clone()]);

        let dir2 = tempfile::tempdir().unwrap();
        git2::Repository::init(dir2.path()).unwrap();
        let new = t.install_session(dir2.path());
        assert!(new.generation > old.generation);
        t.engine.inner.emit_scoped(&old, scoped);
        t.engine.inner.emit(w.clone());
        assert_eq!(t.sink.take(), vec![w]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_active_aborts_only_that_sessions_remote_ops() {
        let t = test_engine();
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        let ctx = t.install_session(dir.path());
        let sessions = &t.engine.inner.sessions;
        let fetch = sessions.begin_remote_op(Some(&ctx));
        let clone = sessions.begin_remote_op(None);
        sessions.cancel_active();
        assert!(ctx.is_cancelled());
        assert!(fetch.flag().load(Ordering::SeqCst));
        assert!(!clone.flag().load(Ordering::SeqCst));
        // A new op on the already-cancelled session starts cancelled.
        assert!(sessions.begin_remote_op(Some(&ctx)).flag().load(Ordering::SeqCst));
        assert_eq!(t.engine.cancel_remote_operation(), 2);
        assert!(clone.flag().load(Ordering::SeqCst));
        drop((fetch, clone));
        assert_eq!(t.engine.cancel_remote_operation(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refresh_emits_every_part_for_the_active_session() {
        let t = test_engine();
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        let ctx = t.install_session(dir.path());
        t.engine.refresh(crate::Refresh::ALL).await.unwrap();
        let names: Vec<&str> = t.sink.take().iter().map(AppEvent::name).collect();
        assert_eq!(
            names,
            [
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
        assert!(!ctx.is_cancelled());
    }
}
