//! Application layer (Wave 2): the [`Engine`] facade the frontend talks to.
//!
//! The engine holds one *active repository session* (D2); git commands act on it and take no `RepoId`.
//! Commands and `open_repository` are serialised so a command cannot race a tab switch (§2.3).
//! Blocking git/SQLite work runs on `spawn_blocking`.
//!
//! Module ownership (one WP per file, so Wave 2 can run in parallel):
//! * `session` — shared plumbing (Wave 2 skeleton): [`session::SessionCtx`], the command lock, `with_repo`, the
//!   remote-operation registry, scoped emission. WP8a implements the lifecycle (`start`, `open_repository`, …).
//! * `refresh` — shared: recompute repo state and emit the matching events (used by commands and watchers).
//! * `progress` — shared: remote-op registration + `TransferProgress` → `AppEvent::RemoteProgress` (git facade, clone).
//! * `bootstrap` — Wave 3: [`Engine::open`] / [`Engine::in_memory`] for the frontend.
//! * `repos`, `settings` — WP8a. `auth` — WP8d. `git` — WP8c. `watch` — WP8b. `cache` — WP8e.

mod auth;
mod bootstrap;
mod cache;
mod git;
mod profiles;
mod progress;
mod refresh;
mod repos;
mod session;
mod settings;
#[cfg(test)]
mod testutil;
mod watch;

use std::sync::Arc;
use std::time::Duration;

use gityas_auth::CredentialStore;
use gityas_core::{EventSink, Oid};
use gityas_storage::Storage;

pub use gityas_auth::ProviderHelp;
pub use profiles::{CarryOver, DEFAULT_PROFILE_ID, MAX_PROFILE_NAME, Profile, ProfileStore, Profiles, clean_name};
pub use refresh::Refresh;
/// Re-exported so the frontend can pass tokens to [`Engine::set_credential`] without depending on `secrecy`.
pub use secrecy::{ExposeSecret, SecretString};

/// What to check out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutTarget {
    /// Full ref name of a local branch, or a remote branch (creates/updates a tracking branch).
    Branch(String),
    /// Detached HEAD at a commit.
    Commit(Oid),
}

/// Tunables. `Default` is what the app ships with; tests shorten the intervals.
#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// Periodic fetch of the active repository (§2 item 7). `None` disables the scheduler. When enabled, this is only
    /// the interval until the settings are read: from then on the scheduler follows `Settings::fetch_interval`.
    pub fetch_interval: Option<Duration>,
    /// Debounce window for the filesystem watchers.
    pub watch_debounce: Duration,
    /// Use the commit-graph cache (`false`: always live-walk; for diagnostics).
    pub use_commit_cache: bool,
    /// Cache GC: unreachable rows older than this are deleted.
    pub cache_gc_max_age: Duration,
    /// Cache GC: size cap of `cache.db` before unreachable rows are deleted regardless of age.
    pub cache_max_bytes: u64,
    /// Cache GC runs at most this often per repository (after a refresh that stored new rows). `ZERO`: every time.
    pub cache_gc_interval: Duration,
    /// Diff size cap and friends (`DiffKind::TooLarge`).
    pub diff: gityas_git::diff::DiffConfig,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            fetch_interval: Some(Duration::from_secs(60)),
            watch_debounce: Duration::from_millis(300),
            use_commit_cache: true,
            cache_gc_max_age: Duration::from_secs(30 * 24 * 60 * 60),
            cache_max_bytes: 512 * 1024 * 1024,
            cache_gc_interval: Duration::from_secs(10 * 60),
            diff: gityas_git::diff::DiffConfig::default(),
        }
    }
}

/// Cheap to clone; all clones share one engine.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

/// Shared state. Background tasks (watchers, scheduler) hold an `Arc<Inner>`, never an `Engine`.
pub(crate) struct Inner {
    pub(crate) storage: Storage,
    pub(crate) credentials: Arc<dyn CredentialStore>,
    pub(crate) events: Arc<dyn EventSink>,
    pub(crate) runtime: tokio::runtime::Handle,
    pub(crate) options: EngineOptions,
    pub(crate) sessions: Arc<session::SessionState>,
    /// Serialises credential writes (`set_credential` / `delete_credential`, WP8d) so each `AuthStatusUpdated`
    /// matches the store's final state. Independent of the command lock: saving a token never waits for a fetch.
    pub(crate) auth_lock: tokio::sync::Mutex<()>,
    /// In-process state of the commit-graph cache (WP8e): hot entry, GC throttle, stats.
    pub(crate) commit_graph: cache::CommitGraphState,
    /// Serialises settings writes (`update_settings`, `modify_settings`, `update_commit_signature`) so a
    /// read-modify-write never loses a concurrent change. Taken only on the blocking pool.
    pub(crate) settings_lock: std::sync::Mutex<()>,
    /// The background fetch interval the scheduler follows (`None`: scheduler disabled). See
    /// [`Inner::follow_fetch_interval`].
    pub(crate) fetch_interval: tokio::sync::watch::Sender<Option<Duration>>,
}

impl Inner {
    /// Makes the running (and future) fetch schedulers use the interval from `settings`, unless the scheduler is
    /// disabled by `EngineOptions::fetch_interval`.
    pub(crate) fn follow_fetch_interval(&self, settings: &gityas_core::models::Settings) {
        if self.options.fetch_interval.is_none() {
            return;
        }
        let wanted = Some(settings.fetch_interval());
        self.fetch_interval.send_if_modified(|current| {
            let changed = *current != wanted;
            *current = wanted;
            changed
        });
    }
}

impl Engine {
    /// Does no IO; call [`Engine::start`] to load state and emit the initial events.
    pub fn new(
        storage: Storage,
        credentials: Arc<dyn CredentialStore>,
        events: Arc<dyn EventSink>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self::with_options(storage, credentials, events, runtime, EngineOptions::default())
    }

    pub fn with_options(
        storage: Storage,
        credentials: Arc<dyn CredentialStore>,
        events: Arc<dyn EventSink>,
        runtime: tokio::runtime::Handle,
        options: EngineOptions,
    ) -> Self {
        let fetch_interval = tokio::sync::watch::Sender::new(options.fetch_interval);
        Self {
            inner: Arc::new(Inner {
                storage,
                credentials,
                events,
                runtime,
                options,
                sessions: Arc::default(),
                auth_lock: tokio::sync::Mutex::new(()),
                commit_graph: cache::CommitGraphState::default(),
                settings_lock: std::sync::Mutex::new(()),
                fetch_interval,
            }),
        }
    }
}
