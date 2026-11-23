//! Settings and commit signature.
//!
//! The signature is `None` until the user sets one (the old default was empty strings, which
//! libgit2 rejects). Whatever is stored has been trimmed and validated with `CommitSignature::validate`; commit/merge
//! read it with `Settings::require_signature`. Settings don't touch the session, so these commands don't take the
//! command lock (storage serialises writes).

use std::sync::Arc;

use gityas_core::models::{AutoCrlf, CommitSignature, Settings};
use gityas_core::{AppEvent, Result};

use crate::Engine;

impl Engine {
    pub async fn settings(&self) -> Result<Settings> {
        let inner = Arc::clone(&self.inner);
        self.inner.blocking(move || inner.storage.user().settings()).await
    }

    /// Replaces every setting (including the signature: `None` clears it). The signature is trimmed and validated
    /// first (`InvalidInput`; nothing is saved). Emits `CommitsSignatureUpdated` if the signature changed to a value.
    pub async fn update_settings(&self, mut settings: Settings) -> Result<()> {
        if let Some(signature) = settings.commit_signature.as_mut() {
            *signature = signature.trimmed();
            signature.validate()?;
        }
        let inner = Arc::clone(&self.inner);
        self.inner
            .blocking(move || {
                let _guard = settings_guard(&inner);
                let user = inner.storage.user();
                let previous = match user.settings() {
                    Ok(previous) => previous.commit_signature,
                    Err(err) => {
                        tracing::warn!("cannot read the previous settings: {err}");
                        None
                    }
                };
                user.save_settings(&settings)?;
                inner.follow_fetch_interval(&settings);
                if let Some(signature) = settings.commit_signature
                    && previous.as_ref() != Some(&signature)
                {
                    inner.emit(AppEvent::CommitsSignatureUpdated(signature));
                }
                Ok(())
            })
            .await
    }

    /// Atomic read-modify-write of the stored settings: `change` runs on the current stored value, serialised with
    /// every other settings write, so concurrent edits (e.g. a theme change while the signature is being saved)
    /// never overwrite each other. A changed signature is trimmed and validated (`InvalidInput`; nothing is saved)
    /// and emitted as `CommitsSignatureUpdated`. Returns the saved settings (there is no settings event).
    pub async fn modify_settings(&self, change: impl FnOnce(&mut Settings) + Send + 'static) -> Result<Settings> {
        let inner = Arc::clone(&self.inner);
        self.inner
            .blocking(move || {
                let _guard = settings_guard(&inner);
                let user = inner.storage.user();
                let mut settings = user.settings()?;
                let previous = settings.commit_signature.clone();
                change(&mut settings);
                let signature_changed = settings.commit_signature != previous;
                if signature_changed && let Some(signature) = settings.commit_signature.as_mut() {
                    *signature = signature.trimmed();
                    signature.validate()?;
                }
                user.save_settings(&settings)?;
                inner.follow_fetch_interval(&settings);
                if signature_changed && let Some(signature) = settings.commit_signature.clone() {
                    inner.emit(AppEvent::CommitsSignatureUpdated(signature));
                }
                Ok(settings)
            })
            .await
    }

    /// Trims, validates (`InvalidInput` with a user-facing message: empty name/email, `<`/`>`, newlines) and stores
    /// the signature. Emits `CommitsSignatureUpdated`.
    pub async fn update_commit_signature(&self, signature: CommitSignature) -> Result<()> {
        let signature = signature.trimmed();
        signature.validate()?;
        let inner = Arc::clone(&self.inner);
        self.inner
            .blocking(move || {
                let _guard = settings_guard(&inner);
                inner.storage.user().set_commit_signature(&signature)?;
                inner.emit(AppEvent::CommitsSignatureUpdated(signature));
                Ok(())
            })
            .await
    }

    /// With no signature stored yet, stores the one from the global git config (`user.name` / `user.email`) and
    /// emits `CommitsSignatureUpdated`. Returns it, or `None` when a signature was already set or git has none.
    /// Real launches call this at startup; it stays editable in Settings.
    pub async fn seed_signature_from_git_config(&self) -> Result<Option<CommitSignature>> {
        let inner = Arc::clone(&self.inner);
        self.inner
            .blocking(move || {
                let _guard = settings_guard(&inner);
                let user = inner.storage.user();
                if user.settings()?.commit_signature.is_some() {
                    return Ok(None);
                }
                let Some(signature) = gityas_git::config::signature()? else {
                    return Ok(None);
                };
                user.set_commit_signature(&signature)?;
                inner.emit(AppEvent::CommitsSignatureUpdated(signature.clone()));
                Ok(Some(signature))
            })
            .await
    }

    /// The effective `core.autocrlf` outside any repository (git config, not a GitYas setting).
    pub async fn autocrlf(&self) -> Result<AutoCrlf> {
        self.inner.blocking(gityas_git::config::autocrlf).await
    }

    /// Sets `core.autocrlf` in the user's global git config (`~/.gitconfig`).
    pub async fn set_global_autocrlf(&self, value: AutoCrlf) -> Result<()> {
        self.inner.blocking(move || gityas_git::config::set_global_autocrlf(value)).await
    }
}

/// A poisoned lock only means another settings write panicked; the stored data is still consistent (SQLite).
fn settings_guard(inner: &crate::Inner) -> std::sync::MutexGuard<'_, ()> {
    inner.settings_lock.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use gityas_core::models::{CommitSignature, DiffViewMode, GraphOptions, Settings, Theme};
    use gityas_core::{AppEvent, GitYasError};

    use crate::testutil::test_engine;

    #[tokio::test(flavor = "multi_thread")]
    async fn saved_fetch_interval_reaches_the_scheduler_with_its_minimum() {
        let t = crate::testutil::test_engine_with(crate::EngineOptions::default());
        let interval = || *t.engine.inner.fetch_interval.borrow();
        t.engine.modify_settings(|s| s.fetch_interval_secs = 20).await.unwrap();
        assert_eq!(interval(), Some(std::time::Duration::from_secs(20)));
        t.engine.modify_settings(|s| s.fetch_interval_secs = 3).await.unwrap();
        assert_eq!(interval(), Some(std::time::Duration::from_secs(15)), "raised to the minimum");

        // Disabled by the options: settings don't turn it on.
        let off = test_engine();
        off.engine.modify_settings(|s| s.fetch_interval_secs = 20).await.unwrap();
        assert_eq!(*off.engine.inner.fetch_interval.borrow(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn signature_is_trimmed_validated_and_emitted() {
        let t = test_engine();
        for (name, email) in [("", "a@b"), ("  ", "a@b"), ("Ada", " "), ("Ada <x>", "a@b"), ("Ada\nX", "a@b")] {
            let err = t.engine.update_commit_signature(CommitSignature::new(name, email)).await;
            assert!(matches!(err, Err(GitYasError::InvalidInput(_))), "{name:?} {email:?}: {err:?}");
        }
        assert!(t.sink.take().is_empty());
        assert_eq!(t.engine.settings().await.unwrap().commit_signature, None);
        assert!(t.engine.settings().await.unwrap().require_signature().is_err());

        t.engine
            .update_commit_signature(CommitSignature::new("  Ada Lovelace ", "\tada@example.com "))
            .await
            .unwrap();
        let expected = CommitSignature::new("Ada Lovelace", "ada@example.com");
        assert_eq!(t.sink.take(), vec![AppEvent::CommitsSignatureUpdated(expected.clone())]);
        let settings = t.engine.settings().await.unwrap();
        assert_eq!(settings.require_signature(), Ok(&expected));
    }

    /// The git config only fills in a missing signature (reading it is tested in `gityas_git::config`).
    #[tokio::test(flavor = "multi_thread")]
    async fn git_config_never_replaces_a_stored_signature() {
        let t = test_engine();
        let mine = CommitSignature::new("Ada", "ada@example.com");
        t.engine.update_commit_signature(mine.clone()).await.unwrap();
        t.sink.take();
        assert_eq!(t.engine.seed_signature_from_git_config().await.unwrap(), None);
        assert!(t.sink.take().is_empty());
        assert_eq!(t.engine.settings().await.unwrap().commit_signature, Some(mine));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn settings_round_trip() {
        let t = test_engine();
        assert_eq!(t.engine.settings().await.unwrap(), Settings::default());
        let settings = Settings {
            commit_signature: Some(CommitSignature::new(" Ada ", "ada@example.com")),
            theme: Theme::Dark,
            diff_view_mode: DiffViewMode::Split,
            graph: GraphOptions {
                show_remote_branches: false,
                show_tags: true,
                show_stashes: false,
                max_commits: 500,
            },
            last_seen_version: Some("0.3.0".into()),
            ..Settings::default()
        };
        t.engine.update_settings(settings.clone()).await.unwrap();
        let signature = CommitSignature::new("Ada", "ada@example.com");
        assert_eq!(t.sink.take(), vec![AppEvent::CommitsSignatureUpdated(signature.clone())]);
        let stored = t.engine.settings().await.unwrap();
        assert_eq!(
            stored,
            Settings {
                commit_signature: Some(signature),
                ..settings.clone()
            }
        );

        // Same signature again: no event. Invalid signature: rejected, nothing saved.
        t.engine
            .update_settings(Settings {
                theme: Theme::Light,
                ..stored.clone()
            })
            .await
            .unwrap();
        assert!(t.sink.take().is_empty());
        assert_eq!(t.engine.settings().await.unwrap().theme, Theme::Light);
        let bad = Settings {
            commit_signature: Some(CommitSignature::new("", "x@y")),
            theme: Theme::System,
            ..stored
        };
        assert!(matches!(t.engine.update_settings(bad).await, Err(GitYasError::InvalidInput(_))));
        assert_eq!(t.engine.settings().await.unwrap().theme, Theme::Light);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn modify_settings_keeps_concurrent_changes() {
        let t = test_engine();
        t.engine
            .update_commit_signature(CommitSignature::new("Ada", "ada@x"))
            .await
            .unwrap();
        t.sink.take();
        let mut tasks = Vec::new();
        for i in 0..20 {
            let engine = t.engine.clone();
            tasks.push(tokio::spawn(async move {
                if i % 2 == 0 {
                    engine.modify_settings(|s| s.graph.show_tags = !s.graph.show_tags).await.map(|_| ())
                } else {
                    engine.modify_settings(|s| s.theme = Theme::Light).await.map(|_| ())
                }
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        let saved = t.engine.settings().await.unwrap();
        // 10 toggles from `true`: back to `true` only if none was lost.
        assert!(saved.graph.show_tags);
        assert_eq!(saved.theme, Theme::Light);
        assert_eq!(saved.commit_signature, Some(CommitSignature::new("Ada", "ada@x")));
        assert!(t.sink.take().is_empty(), "signature unchanged: nothing emitted");

        let err = t
            .engine
            .modify_settings(|s| s.commit_signature = Some(CommitSignature::new(" ", "x")))
            .await;
        assert!(matches!(err, Err(GitYasError::InvalidInput(_))));
        let renamed = t
            .engine
            .modify_settings(|s| s.commit_signature = Some(CommitSignature::new(" Bob ", "b@x")))
            .await
            .unwrap();
        assert_eq!(renamed.commit_signature, Some(CommitSignature::new("Bob", "b@x")));
        assert_eq!(
            t.sink.take(),
            [AppEvent::CommitsSignatureUpdated(CommitSignature::new("Bob", "b@x"))]
        );
    }
}
