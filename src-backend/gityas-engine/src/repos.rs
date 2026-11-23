//! WP8a: repository registry and the session lifecycle (start / shutdown / open / close / tabs).
//!
//! Locking (see `session.rs`): only operations that touch the active session (`open_repository`,
//! `close_repository`, `delete_repository`, `set_repository_provider`, `shutdown`) take the command lock. Pure
//! registry/tab changes (`add_repository`, `reorder_repositories`, `set_open_tabs`) run on the blocking pool without
//! it, so they are never stuck behind a long fetch holding the lock; storage serialises them itself. `clone_repository`
//! takes no lock for the transfer (it touches no session).

mod clone;
mod register;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gityas_core::models::{OpenTabs, Repository};
use gityas_core::{AppEvent, GitYasError, Provider, RepoId, Result};

use crate::session::{Session, SessionCtx};
use crate::{Engine, Inner, Refresh, cache, refresh, watch};

impl Engine {
    /// Reports the non-fatal recoveries from `gityas_storage::Storage::open` (`OpenReport::warnings`) as
    /// `AppEvent::Warning`. Call it right after constructing the engine (before or after [`Engine::start`]); the
    /// events go to the sink immediately, so the UI shows them on its first frame either way.
    pub fn report_storage_warnings(&self, warnings: Vec<String>) {
        for warning in warnings {
            tracing::warn!("storage: {warning}");
            self.inner.emit(AppEvent::Warning(warning));
        }
    }

    /// Emits `RepositoriesUpdated`, the commit signature (if set) and auth status, then reopens the last active
    /// repository. A repository that can no longer be opened (folder moved/deleted, not a repo any more) does not fail
    /// start: a `Warning` is emitted, its tab is closed (it stays registered) and the next tab is tried. Emits
    /// `SelectedRepositoryUpdated(None)` when nothing is reopened. Storage errors are returned. `Cancelled` (nothing
    /// emitted) after [`Engine::shutdown`].
    pub async fn start(&self) -> Result<()> {
        if self.inner.sessions.is_shut_down() {
            return Err(GitYasError::Cancelled);
        }
        let inner = Arc::clone(&self.inner);
        let (repos, settings, auth, tabs) = self
            .inner
            .blocking(move || {
                let user = inner.storage.user();
                let repos = user.repositories()?;
                let settings = user.settings()?;
                let auth = inner.credentials.status();
                let stored = user.open_tabs()?;
                let tabs = normalize_tabs(stored.clone(), &repos);
                if tabs != stored {
                    persist_tabs(&inner, &tabs);
                }
                Ok((repos, settings, auth, tabs))
            })
            .await?;

        self.inner.follow_fetch_interval(&settings);
        self.inner.emit(AppEvent::RepositoriesUpdated(repos.clone()));
        if let Some(signature) = settings.commit_signature {
            self.inner.emit(AppEvent::CommitsSignatureUpdated(signature));
        }
        for (provider, connected) in auth {
            self.inner.emit(AppEvent::AuthStatusUpdated { provider, connected });
        }

        let mut tabs = tabs;
        loop {
            let Some(id) = tabs.active.clone() else {
                self.inner.emit(AppEvent::SelectedRepositoryUpdated(None));
                return Ok(());
            };
            match self.open_repository(id.clone()).await {
                Ok(()) => return Ok(()),
                Err(GitYasError::Cancelled) => return Err(GitYasError::Cancelled),
                Err(err) => {
                    let name = repos.iter().find(|r| r.id == id).map_or(id.as_str(), |r| r.name.as_str());
                    tracing::warn!(repo = %id, "cannot reopen repository at startup: {err}");
                    self.inner
                        .emit(AppEvent::Warning(format!("Could not reopen {name}: {err}. Its tab was closed.")));
                    tabs.close(&id);
                    let inner = Arc::clone(&self.inner);
                    let snapshot = tabs.clone();
                    self.inner
                        .blocking(move || {
                            persist_tabs(&inner, &snapshot);
                            Ok(())
                        })
                        .await?;
                }
            }
        }
    }

    /// Aborts every remote operation (including a clone), cancels the session's tasks (watchers, fetch scheduler)
    /// and waits for them to stop. Idempotent. Open tabs are kept so the next start restores them.
    ///
    /// Terminal for sessions: afterwards [`Engine::open_repository`] and [`Engine::start`] fail with
    /// `GitYasError::Cancelled` (also a switch that was already waiting when shutdown ran), so no watcher or
    /// scheduler outlives it; repository commands fail with `NoActiveRepository`. Registry, settings and credential
    /// commands keep working.
    pub async fn shutdown(&self) -> Result<()> {
        self.inner.sessions.mark_shut_down();
        self.inner.sessions.cancel_remote_ops();
        self.inner.sessions.cancel_active();
        let _guard = self.inner.sessions.command_lock.lock().await;
        if let Some(old) = self.inner.sessions.replace(None) {
            old.stop().await;
        }
        Ok(())
    }

    pub async fn repositories(&self) -> Result<Vec<Repository>> {
        let inner = Arc::clone(&self.inner);
        self.inner.blocking(move || inner.storage.user().repositories()).await
    }

    /// Registers an existing repository. `path` may be the root, a subfolder of the working tree, its `.git` folder,
    /// a linked worktree or a bare repository; the canonical root is registered. The provider is detected from the
    /// remote URL (`origin` first) unless given. `AlreadyExists` if that root is registered, `NotFound` if `path` is
    /// not in a repository, `InvalidInput` for non-UTF-8 paths. Emits `RepositoriesUpdated`.
    pub async fn add_repository(&self, path: PathBuf, provider: Option<Provider>) -> Result<Repository> {
        let inner = Arc::clone(&self.inner);
        self.inner
            .blocking(move || {
                let resolved = register::resolve(&path, provider)?;
                insert(&inner, resolved)
            })
            .await
    }

    /// Where the clone dialog's "Into" folder and a URL put the clone: `base_dir/<name from URL>`. Pure.
    pub fn clone_destination(&self, url: &str, base_dir: &Path) -> Result<PathBuf> {
        gityas_git::repository::clone_destination(url, base_dir)
    }

    /// Hosted provider a remote / clone URL points at (github.com, gitlab.com, bitbucket.org, dev.azure.com /
    /// visualstudio.com; https, ssh and scp-like forms), `None` for self-hosted or local URLs. Pure; the clone dialog
    /// uses it to show "GitHub (detected)" and the missing-token hint.
    pub fn detect_provider(&self, url: &str) -> Option<Provider> {
        gityas_git::remote::detect_provider(url)
    }

    /// Clones `url` into `path` (the final repository folder, which must not exist or be empty; see
    /// [`Engine::clone_destination`]) and registers it. Not tied to the session: reports
    /// `AppEvent::RemoteProgress { repo: None, op: Clone }` and is aborted by [`Engine::cancel_remote_operation`]
    /// (`Cancelled`). On failure or cancel, what this call created is removed again (a pre-existing empty folder is
    /// emptied, not deleted). Does not open the repository. Emits `RepositoriesUpdated`.
    pub async fn clone_repository(&self, url: String, path: PathBuf, provider: Option<Provider>) -> Result<Repository> {
        let inner = Arc::clone(&self.inner);
        let op = self.inner.sessions.begin_remote_op(None);
        self.inner
            .blocking(move || {
                let dest = clone::clone_blocking(&inner, &url, &path, provider, op.flag())?;
                drop(op);
                let provider = provider.or_else(|| gityas_git::remote::detect_provider(&url));
                let resolved = register::resolve(&dest, provider)?;
                insert(&inner, resolved)
            })
            .await
    }

    /// Changes the stored provider (`None`: unknown / local only); the active session uses it from its next command.
    /// Emits `RepositoriesUpdated`.
    pub async fn set_repository_provider(&self, id: RepoId, provider: Option<Provider>) -> Result<()> {
        self.with_lock(move |inner| {
            inner.storage.user().update_repository_provider(&id, provider)?;
            inner.sessions.set_active_provider(&id, provider);
            emit_repositories(inner);
            Ok(())
        })
        .await
    }

    /// `ordered` must list every registered id exactly once (`InvalidInput` otherwise). Emits `RepositoriesUpdated`.
    pub async fn reorder_repositories(&self, ordered: Vec<RepoId>) -> Result<()> {
        let inner = Arc::clone(&self.inner);
        self.inner
            .blocking(move || {
                inner.storage.user().reorder_repositories(&ordered)?;
                emit_repositories(&inner);
                Ok(())
            })
            .await
    }

    /// Unregisters (never deletes files), closes its tab and purges its cache rows. If it is the active repository
    /// the session is closed first (`SelectedRepositoryUpdated(None)`). Emits `RepositoriesUpdated`.
    pub async fn delete_repository(&self, id: RepoId) -> Result<()> {
        if self.inner.sessions.active().is_some_and(|ctx| ctx.repo_id == id) {
            self.inner.sessions.cancel_active();
        }
        let _guard = self.inner.sessions.command_lock.lock().await;
        let inner = Arc::clone(&self.inner);
        let lookup = id.clone();
        self.inner
            .blocking(move || {
                inner.storage.user().repository(&lookup)?.ok_or_else(|| not_found(&lookup))?;
                Ok(())
            })
            .await?;
        if self.inner.sessions.active().is_some_and(|ctx| ctx.repo_id == id) {
            if let Some(old) = self.inner.sessions.replace(None) {
                old.stop().await;
            }
            self.inner.emit(AppEvent::SelectedRepositoryUpdated(None));
        }
        let inner = Arc::clone(&self.inner);
        self.inner
            .blocking(move || {
                // Storage closes its tab and clears the selection in the same transaction (foreign keys).
                inner.storage.user().delete_repository(&id)?;
                if let Err(err) = cache::purge_repository(&inner, &id) {
                    tracing::warn!(repo = %id, "cannot purge cached history of a deleted repository: {err}");
                }
                emit_repositories(&inner);
                Ok(())
            })
            .await
    }

    /// Switches the active session: cancels the previous session (and its in-flight remote operations), starts
    /// watchers and the fetch scheduler, then emits `SelectedRepositoryUpdated` and the full repo state (commits,
    /// head, branches, remotes, files), and records the tab as open and active. `NotFound` for an unknown id or a
    /// repository that can no longer be opened; the current session is left untouched in that case.
    /// Re-opening the active repository keeps its session and only re-emits the state. `Cancelled` after
    /// [`Engine::shutdown`].
    pub async fn open_repository(&self, id: RepoId) -> Result<()> {
        if self
            .inner
            .sessions
            .active()
            .is_some_and(|ctx| ctx.repo_id == id && !ctx.is_cancelled())
        {
            let _guard = self.inner.sessions.command_lock.lock().await;
            if self.inner.sessions.is_shut_down() {
                return Err(GitYasError::Cancelled);
            }
            if let Some(ctx) = self.inner.sessions.active().filter(|ctx| ctx.repo_id == id && !ctx.is_cancelled()) {
                return self.reemit(ctx).await;
            }
        }

        // Validate before touching the current session, so a bad id or a vanished folder doesn't end it.
        let inner = Arc::clone(&self.inner);
        let lookup = id.clone();
        self.inner.blocking(move || openable(&inner, &lookup).map(drop)).await?;

        self.inner.sessions.cancel_active();
        let _guard = self.inner.sessions.command_lock.lock().await;
        if self.inner.sessions.is_shut_down() {
            return Err(GitYasError::Cancelled);
        }
        // Re-read under the lock: the record (provider) may have changed while we waited.
        let inner = Arc::clone(&self.inner);
        let record = match self.inner.blocking(move || openable(&inner, &id)).await {
            Ok(record) => record,
            Err(err) => {
                self.end_session_locked().await;
                return Err(err);
            }
        };

        let sessions = &self.inner.sessions;
        let ctx = sessions.new_ctx(record.id.clone(), record.path.clone(), record.provider);
        if let Some(old) = sessions.replace(Some(Session {
            ctx: ctx.clone(),
            tasks: Vec::new(),
        })) {
            old.stop().await;
        }
        match watch::start_session_tasks(&self.inner, &ctx) {
            Ok(tasks) => tasks.into_iter().for_each(|task| sessions.attach_task(&ctx, task)),
            Err(err) => {
                tracing::warn!(repo = %ctx.repo_id, "cannot start watchers: {err}");
                self.inner
                    .emit(AppEvent::Warning(format!("{} will not refresh automatically: {err}", record.name)));
            }
        }
        self.inner.emit(AppEvent::SelectedRepositoryUpdated(Some(ctx.repo_id.clone())));

        let inner = Arc::clone(&self.inner);
        let session = ctx.clone();
        let opened = self
            .inner
            .blocking(move || {
                record_open_tab(&inner, &session.repo_id);
                let repo = gityas_git::repository::open(&session.path)?;
                log_refresh(&session, refresh::refresh_blocking(&inner, &session, &repo, Refresh::ALL));
                Ok(())
            })
            .await;
        if let Err(err) = opened {
            // The folder vanished between validation and now: don't leave a session on a missing repository.
            if self.inner.sessions.is_current(&ctx) {
                self.end_session_locked().await;
            }
            return Err(err);
        }
        Ok(())
    }

    /// Ends the active session without opening another (`SelectedRepositoryUpdated(None)`). The tab stays open but
    /// no repository is recorded as active, so the next start does not reopen one. No-op without a session.
    pub async fn close_repository(&self) -> Result<()> {
        self.inner.sessions.cancel_active();
        let _guard = self.inner.sessions.command_lock.lock().await;
        let Some(old) = self.inner.sessions.replace(None) else {
            return Ok(());
        };
        old.stop().await;
        self.inner.emit(AppEvent::SelectedRepositoryUpdated(None));
        let inner = Arc::clone(&self.inner);
        self.inner
            .blocking(move || {
                let mut tabs = inner.storage.user().open_tabs()?;
                tabs.active = None;
                persist_tabs(&inner, &tabs);
                Ok(())
            })
            .await
    }

    /// Id of the active repository, if any.
    pub fn active_repository(&self) -> Option<RepoId> {
        self.inner.sessions.active().map(|ctx| ctx.repo_id)
    }

    /// Persisted tabs, repaired against the registry (unknown ids and duplicates dropped).
    pub async fn open_tabs(&self) -> Result<OpenTabs> {
        let inner = Arc::clone(&self.inner);
        self.inner
            .blocking(move || {
                let user = inner.storage.user();
                let repos = user.repositories()?;
                Ok(normalize_tabs(user.open_tabs()?, &repos))
            })
            .await
    }

    /// Persists the tab bar. Unknown ids and duplicates are dropped and an `active` that isn't open falls back to the
    /// first tab (`None` stays `None`). Does not switch the session: call [`Engine::open_repository`] for that.
    pub async fn set_open_tabs(&self, tabs: OpenTabs) -> Result<()> {
        let inner = Arc::clone(&self.inner);
        self.inner
            .blocking(move || {
                let user = inner.storage.user();
                let tabs = normalize_tabs(tabs, &user.repositories()?);
                user.set_open_tabs(&tabs)
            })
            .await
    }

    /// Fast path of `open_repository` for the already-active repository (command lock held).
    async fn reemit(&self, ctx: SessionCtx) -> Result<()> {
        self.inner.emit(AppEvent::SelectedRepositoryUpdated(Some(ctx.repo_id.clone())));
        let inner = Arc::clone(&self.inner);
        self.inner
            .blocking(move || {
                record_open_tab(&inner, &ctx.repo_id);
                let repo = gityas_git::repository::open(&ctx.path)?;
                log_refresh(&ctx, refresh::refresh_blocking(&inner, &ctx, &repo, Refresh::ALL));
                Ok(())
            })
            .await
    }

    /// Stops and clears the active session (command lock held), emitting `SelectedRepositoryUpdated(None)` if
    /// there was one.
    async fn end_session_locked(&self) {
        if let Some(old) = self.inner.sessions.replace(None) {
            old.stop().await;
            self.inner.emit(AppEvent::SelectedRepositoryUpdated(None));
        }
    }
}

/// The registered repository `id`, checked to still open as a git repository. Blocking.
fn openable(inner: &Inner, id: &RepoId) -> Result<Repository> {
    let record = inner.storage.user().repository(id)?.ok_or_else(|| not_found(id))?;
    match gityas_git::repository::open(&record.path) {
        Ok(_) => Ok(record),
        Err(GitYasError::NotFound(_)) if !record.path.exists() => Err(GitYasError::NotFound(format!(
            "the folder {} no longer exists",
            record.path.display()
        ))),
        Err(GitYasError::NotFound(_)) => Err(GitYasError::NotFound(format!(
            "{} is no longer a git repository",
            record.path.display()
        ))),
        Err(err) => Err(err),
    }
}

fn not_found(id: &RepoId) -> GitYasError {
    GitYasError::NotFound(format!("repository {id}"))
}

/// Inserts a resolved repository and emits `RepositoriesUpdated`. Blocking.
fn insert(inner: &Inner, resolved: register::Resolved) -> Result<Repository> {
    let repo = inner
        .storage
        .user()
        .insert_repository(&resolved.name, &resolved.path, resolved.provider)
        .map_err(|err| match err {
            GitYasError::AlreadyExists(_) => GitYasError::AlreadyExists(format!("{} is already in GitYas", resolved.path.display())),
            other => other,
        })?;
    emit_repositories(inner);
    Ok(repo)
}

/// Emits the registry after a change. A read failure after a successful change is logged, not returned.
fn emit_repositories(inner: &Inner) {
    match inner.storage.user().repositories() {
        Ok(repos) => inner.emit(AppEvent::RepositoriesUpdated(repos)),
        Err(err) => tracing::error!("cannot reload repositories after a change: {err}"),
    }
}

/// `OpenTabs::normalize` against the registry, except that "no active repository" is preserved.
fn normalize_tabs(mut tabs: OpenTabs, repos: &[Repository]) -> OpenTabs {
    let known: Vec<&str> = repos.iter().map(|r| r.id.as_str()).collect();
    let no_active = tabs.active.is_none();
    tabs.normalize(&known);
    if no_active {
        tabs.active = None;
    }
    tabs
}

/// Opens `id`'s tab and marks it active in storage. Tabs are UI convenience: failures are logged. Blocking.
fn record_open_tab(inner: &Inner, id: &RepoId) {
    match inner.storage.user().open_tabs() {
        Ok(mut tabs) => {
            tabs.open(id.clone());
            persist_tabs(inner, &tabs);
        }
        Err(err) => tracing::warn!(repo = %id, "cannot load open tabs: {err}"),
    }
}

fn persist_tabs(inner: &Inner, tabs: &OpenTabs) {
    if let Err(err) = inner.storage.user().set_open_tabs(tabs) {
        tracing::warn!("cannot save open tabs: {err}");
    }
}

fn log_refresh(ctx: &SessionCtx, result: Result<()>) {
    if let Err(err) = result {
        tracing::warn!(repo = %ctx.repo_id, "part of the repository state could not be loaded: {err}");
    }
}
