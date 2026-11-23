//! Constructors for the frontend, which may depend only on `gityas-engine`, `gityas-core`
//! and `gityas-platform`, so it cannot name `Storage` or a `CredentialStore` itself.

use std::sync::Arc;

use gityas_auth::{KeyringStore, MemoryCredentialStore};
use gityas_core::{EventSink, Result};
use gityas_platform::Paths;
use gityas_storage::{Storage, StoragePaths};

use crate::{Engine, EngineOptions};

impl Engine {
    /// The shipped configuration: SQLite databases under `paths` and the OS keychain. Storage recoveries (corrupt
    /// user DB renamed, cache rebuilt) are emitted as `AppEvent::Warning` right away. Call [`Engine::start`] next.
    pub fn open(paths: &Paths, events: Arc<dyn EventSink>, runtime: tokio::runtime::Handle) -> Result<Engine> {
        let storage_paths = StoragePaths {
            data_dir: paths.data_dir.clone(),
            cache_dir: paths.cache_dir.clone(),
        };
        let (storage, report) = Storage::open(&storage_paths)?;
        let engine = Engine::new(storage, Arc::new(KeyringStore::new()), events, runtime);
        engine.report_storage_warnings(report.warnings);
        Ok(engine)
    }

    /// In-memory databases and credentials, nothing persisted (the frontend's `--mock` mode and UI tests).
    /// The fetch scheduler is off.
    pub fn in_memory(events: Arc<dyn EventSink>, runtime: tokio::runtime::Handle) -> Result<Engine> {
        let options = EngineOptions {
            fetch_interval: None,
            ..EngineOptions::default()
        };
        Ok(Engine::with_options(
            Storage::in_memory()?,
            Arc::new(MemoryCredentialStore::new()),
            events,
            runtime,
            options,
        ))
    }
}

#[cfg(test)]
mod tests {
    use gityas_core::events::RecordingSink;
    use gityas_core::{AppEvent, GitYasError};

    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_engine_starts_empty() -> Result<()> {
        let sink = Arc::new(RecordingSink::new());
        let engine = Engine::in_memory(sink.clone(), tokio::runtime::Handle::current())?;
        engine.start().await?;
        assert!(engine.repositories().await?.is_empty());
        assert!(matches!(engine.head().await, Err(GitYasError::NoActiveRepository)));
        let events = sink.take();
        assert!(events.contains(&AppEvent::RepositoriesUpdated(vec![])), "{events:?}");
        engine.shutdown().await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_creates_databases_under_paths() -> Result<()> {
        let dir = tempfile::tempdir().map_err(|e| GitYasError::Io(e.to_string()))?;
        let paths = Paths::under(dir.path());
        paths.create_dirs()?;
        let engine = Engine::open(&paths, Arc::new(RecordingSink::new()), tokio::runtime::Handle::current())?;
        engine.start().await?;
        assert!(paths.data_dir.join("gityas.db").exists());
        engine.shutdown().await
    }
}
