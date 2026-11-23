//! Test helpers shared by the engine's unit tests. They let each module test against an active
//! session without going through `open_repository`: [`install_session`] installs one directly, with no
//! watchers or scheduler.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gityas_auth::MemoryCredentialStore;
use gityas_core::events::RecordingSink;
use gityas_core::models::Repository;
use gityas_storage::Storage;

use crate::session::{Session, SessionCtx};
use crate::{Engine, EngineOptions};

/// `file://` URL for a local path: `file:///C:/dir/repo.git` on Windows, `file:///tmp/repo.git` elsewhere.
pub(crate) fn file_url(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.strip_prefix("//?/").unwrap_or(&text);
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        format!("file:///{text}")
    }
}

pub(crate) struct TestEngine {
    pub(crate) engine: Engine,
    pub(crate) sink: Arc<RecordingSink>,
    pub(crate) credentials: Arc<MemoryCredentialStore>,
}

/// In-memory storage, memory credentials, recording sink; scheduler disabled and short debounce.
/// Call from inside a multi-threaded tokio runtime (`#[tokio::test(flavor = "multi_thread")]`).
pub(crate) fn test_engine() -> TestEngine {
    test_engine_with(EngineOptions {
        fetch_interval: None,
        watch_debounce: std::time::Duration::from_millis(50),
        ..EngineOptions::default()
    })
}

pub(crate) fn test_engine_with(options: EngineOptions) -> TestEngine {
    let storage = Storage::in_memory().unwrap_or_else(|e| panic!("in-memory storage: {e}"));
    let sink = Arc::new(RecordingSink::new());
    let credentials = Arc::new(MemoryCredentialStore::new());
    let engine = Engine::with_options(
        storage,
        credentials.clone(),
        sink.clone(),
        tokio::runtime::Handle::current(),
        options,
    );
    TestEngine { engine, sink, credentials }
}

impl TestEngine {
    /// Registers `path` in the user DB and installs it as the active session (no watchers, no events).
    pub(crate) fn install_session(&self, path: &Path) -> SessionCtx {
        let name = Repository::default_name(path).unwrap_or_else(|| "repo".into());
        let repo = self
            .engine
            .inner
            .storage
            .user()
            .insert_repository(&name, path, None)
            .unwrap_or_else(|e| panic!("insert repository: {e}"));
        let ctx = self.engine.inner.sessions.new_ctx(repo.id, PathBuf::from(path), repo.provider);
        let previous = self.engine.inner.sessions.replace(Some(Session {
            ctx: ctx.clone(),
            tasks: Vec::new(),
        }));
        if let Some(previous) = previous {
            previous.ctx.cancel.cancel();
        }
        ctx
    }
}
