use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use gityas_core::{Credential, CredentialProvider, Provider, Result};

use crate::{CredentialStore, normalize_credential};

/// In-memory store for tests and `--mock` runs. Applies the same validation as [`crate::KeyringStore`].
#[derive(Debug, Default)]
pub struct MemoryCredentialStore {
    credentials: Mutex<HashMap<Provider, Credential>>,
}

impl MemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Provider, Credential>> {
        // The map is never left half-updated (each operation is a single insert/remove), so a poisoned lock is safe
        // to keep using.
        self.credentials.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl CredentialProvider for MemoryCredentialStore {
    fn credential(&self, provider: Provider) -> Option<Credential> {
        self.lock().get(&provider).cloned()
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn set(&self, provider: Provider, credential: Credential) -> Result<()> {
        let credential = normalize_credential(provider, credential)?;
        self.lock().insert(provider, credential);
        Ok(())
    }

    fn delete(&self, provider: Provider) -> Result<()> {
        self.lock().remove(&provider);
        Ok(())
    }

    fn status(&self) -> Vec<(Provider, bool)> {
        let map = self.lock();
        Provider::ALL.into_iter().map(|p| (p, map.contains_key(&p))).collect()
    }

    fn try_credential(&self, provider: Provider) -> Result<Option<Credential>> {
        Ok(self.credential(provider))
    }
}

#[cfg(test)]
mod tests {
    use gityas_core::GitYasError;
    use secrecy::{ExposeSecret, SecretString};

    use super::*;

    const TOKEN: &str = "glpat-SuperSecretValue123";

    fn cred(username: Option<&str>, token: &str) -> Credential {
        Credential {
            username: username.map(str::to_owned),
            token: SecretString::from(token),
        }
    }

    #[test]
    fn starts_disconnected_in_provider_order() {
        let store = MemoryCredentialStore::new();
        let status = store.status();
        assert_eq!(status.iter().map(|(p, _)| *p).collect::<Vec<_>>(), Provider::ALL.to_vec());
        assert!(status.iter().all(|(_, c)| !c));
        for p in Provider::ALL {
            assert!(store.credential(p).is_none());
            assert!(store.try_credential(p).unwrap().is_none());
        }
    }

    #[test]
    fn set_get_replace_delete() {
        let store = MemoryCredentialStore::new();
        store.set(Provider::Gitlab, cred(Some(" alice "), &format!(" {TOKEN}\n"))).unwrap();

        let got = store.credential(Provider::Gitlab).unwrap();
        assert_eq!(got.username.as_deref(), Some("alice"));
        assert_eq!(got.token.expose_secret(), TOKEN);
        assert_eq!(
            store.status(),
            vec![
                (Provider::Github, false),
                (Provider::Gitlab, true),
                (Provider::Bitbucket, false),
                (Provider::AzureDevops, false)
            ]
        );

        store.set(Provider::Gitlab, cred(None, "second")).unwrap();
        let got = store.try_credential(Provider::Gitlab).unwrap().unwrap();
        assert_eq!(got.username, None);
        assert_eq!(got.token.expose_secret(), "second");

        store.delete(Provider::Gitlab).unwrap();
        assert!(store.credential(Provider::Gitlab).is_none());
        store.delete(Provider::Gitlab).unwrap(); // idempotent
        store.delete(Provider::Bitbucket).unwrap();
        assert!(store.status().iter().all(|(_, c)| !c));
    }

    #[test]
    fn providers_are_independent() {
        let store = MemoryCredentialStore::new();
        for (i, p) in Provider::ALL.into_iter().enumerate() {
            store.set(p, cred(None, &format!("token-{i}"))).unwrap();
        }
        assert!(store.status().iter().all(|(_, c)| *c));
        store.delete(Provider::Bitbucket).unwrap();
        for (i, p) in Provider::ALL.into_iter().enumerate() {
            let got = store.credential(p);
            if p == Provider::Bitbucket {
                assert!(got.is_none());
            } else {
                assert_eq!(got.unwrap().token.expose_secret(), format!("token-{i}"));
            }
        }
    }

    #[test]
    fn invalid_credential_is_rejected_and_keeps_previous() {
        let store = MemoryCredentialStore::new();
        store.set(Provider::Github, cred(None, TOKEN)).unwrap();
        let err = store.set(Provider::Github, cred(None, "   ")).unwrap_err();
        assert!(matches!(err, GitYasError::InvalidInput(_)));
        assert_eq!(store.credential(Provider::Github).unwrap().token.expose_secret(), TOKEN);
    }

    #[test]
    fn debug_does_not_leak_token() {
        let store = MemoryCredentialStore::new();
        store.set(Provider::Github, cred(Some("alice"), TOKEN)).unwrap();
        let shown = format!("{store:?} {:?}", store.credential(Provider::Github));
        assert!(!shown.contains(TOKEN) && !shown.contains("SuperSecret"), "{shown}");
        assert!(shown.contains("alice"));
    }

    #[test]
    fn usable_as_trait_objects() {
        let store: std::sync::Arc<dyn CredentialStore> = std::sync::Arc::new(MemoryCredentialStore::new());
        store.set(Provider::AzureDevops, cred(None, TOKEN)).unwrap();
        let provider: std::sync::Arc<dyn CredentialProvider> = std::sync::Arc::new(MemoryCredentialStore::new());
        assert!(provider.credential(Provider::AzureDevops).is_none());
        assert!(store.credential(Provider::AzureDevops).is_some());
    }
}
