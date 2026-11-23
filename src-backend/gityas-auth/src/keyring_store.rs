use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use gityas_core::{Credential, CredentialProvider, GitYasError, Provider, Result};
use keyring::credential::CredentialPersistence;
use zeroize::{Zeroize, Zeroizing};

use crate::{CredentialStore, KEYCHAIN_SERVICE, codec, keychain_account, normalize_credential};

/// OS keychain backed store (macOS Keychain, Windows Credential Manager, Secret Service on Linux/BSD).
///
/// No plaintext fallback: if the platform has no persistent keychain backend (keyring would silently use its
/// in-memory mock), or the keychain is unavailable/locked, operations return `GitYasError::Auth` with a clear message.
#[derive(Debug)]
pub struct KeyringStore {
    service: String,
    persistent: bool,
    /// What each provider's entry held when this process last read or wrote it (`None` = no entry). Every item is
    /// read from the keychain at most once per launch: on macOS each read of an item the running binary isn't
    /// trusted for (every rebuild or update of an ad hoc signed app) asks for the login password. Errors are not
    /// cached, so a locked keychain is retried.
    cache: Mutex<HashMap<Provider, Option<Credential>>>,
}

impl Default for KeyringStore {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Read,
    Save,
    Remove,
}

impl Op {
    fn verb(self) -> &'static str {
        match self {
            Op::Read => "read",
            Op::Save => "save",
            Op::Remove => "remove",
        }
    }
}

impl KeyringStore {
    /// The production store (service [`KEYCHAIN_SERVICE`]).
    pub fn new() -> Self {
        Self::with_service(KEYCHAIN_SERVICE)
    }

    /// A store under a different keychain service name (tests, side-by-side dev builds).
    pub fn with_service(service: impl Into<String>) -> Self {
        let persistent = matches!(
            keyring::default::default_credential_builder().persistence(),
            CredentialPersistence::UntilDelete
        );
        Self {
            service: service.into(),
            persistent,
            cache: Mutex::default(),
        }
    }

    fn cache(&self) -> MutexGuard<'_, HashMap<Provider, Option<Credential>>> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    /// Whether this build has a keychain backend that keeps credentials across restarts.
    pub fn is_backend_persistent(&self) -> bool {
        self.persistent
    }

    fn entry(&self, provider: Provider, op: Op) -> Result<keyring::Entry> {
        if !self.persistent {
            return Err(GitYasError::Auth(format!(
                "Could not {} the {provider} token: no system keychain is available on this platform. \
                 GitYas does not store tokens in plain text.",
                op.verb()
            )));
        }
        keyring::Entry::new(&self.service, &keychain_account(provider)).map_err(|e| map_keyring_error(op, provider, &e))
    }
}

impl CredentialProvider for KeyringStore {
    fn credential(&self, provider: Provider) -> Option<Credential> {
        match self.try_credential(provider) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(provider = provider.key(), error = %e, "keychain lookup failed; treating provider as not connected");
                None
            }
        }
    }
}

impl CredentialStore for KeyringStore {
    fn set(&self, provider: Provider, credential: Credential) -> Result<()> {
        let credential = normalize_credential(provider, credential)?;
        let entry = self.entry(provider, Op::Save)?;
        let json = codec::encode(&credential)?;
        entry.set_password(&json).map_err(|e| map_keyring_error(Op::Save, provider, &e))?;
        self.cache().insert(provider, Some(credential));
        tracing::info!(provider = provider.key(), "saved token to the system keychain");
        Ok(())
    }

    fn delete(&self, provider: Provider) -> Result<()> {
        let entry = self.entry(provider, Op::Remove)?;
        let result = match entry.delete_credential() {
            Ok(()) => {
                tracing::info!(provider = provider.key(), "removed token from the system keychain");
                Ok(())
            }
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(map_keyring_error(Op::Remove, provider, &e)),
        };
        if result.is_ok() {
            self.cache().insert(provider, None);
        }
        result
    }

    fn status(&self) -> Vec<(Provider, bool)> {
        Provider::ALL.into_iter().map(|p| (p, self.credential(p).is_some())).collect()
    }

    fn try_credential(&self, provider: Provider) -> Result<Option<Credential>> {
        if let Some(cached) = self.cache().get(&provider) {
            return Ok(cached.clone());
        }
        let found = self.read(provider)?;
        self.cache().insert(provider, found.clone());
        Ok(found)
    }
}

impl KeyringStore {
    /// Reads `provider`'s entry from the keychain (an unreadable value counts as no entry).
    fn read(&self, provider: Provider) -> Result<Option<Credential>> {
        let entry = self.entry(provider, Op::Read)?;
        match entry.get_password() {
            Ok(raw) => {
                let raw = Zeroizing::new(raw);
                match codec::decode(&raw) {
                    Ok(c) => Ok(Some(c)),
                    Err(e) => {
                        tracing::warn!(
                            provider = provider.key(),
                            reason = %e,
                            "stored keychain value is unreadable; treating provider as not connected (saving a new token replaces it)"
                        );
                        Ok(None)
                    }
                }
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(keyring::Error::BadEncoding(mut bytes)) => {
                bytes.zeroize();
                tracing::warn!(
                    provider = provider.key(),
                    "stored keychain value is not UTF-8; treating provider as not connected (saving a new token replaces it)"
                );
                Ok(None)
            }
            Err(e) => Err(map_keyring_error(Op::Read, provider, &e)),
        }
    }
}

/// Maps a keyring error to a user-facing `GitYasError::Auth`. Never includes credential data: `BadEncoding` holds
/// the stored bytes and `Ambiguous`'s `Display` formats the matching credentials, so neither is formatted.
fn map_keyring_error(op: Op, provider: Provider, err: &keyring::Error) -> GitYasError {
    let detail = match err {
        keyring::Error::PlatformFailure(e) => format!("the system keychain reported an error: {e}"),
        keyring::Error::NoStorageAccess(e) => format!("the system keychain is locked or access was denied: {e}"),
        keyring::Error::NoEntry => "no saved token was found".to_owned(),
        keyring::Error::BadEncoding(_) => "the saved value is not valid text".to_owned(),
        keyring::Error::TooLong(attr, limit) => format!("the keychain {attr} is longer than the platform limit of {limit}"),
        keyring::Error::Invalid(attr, reason) => format!("the keychain {attr} is invalid: {reason}"),
        keyring::Error::Ambiguous(items) => {
            format!(
                "{} matching keychain entries were found; remove the duplicates in your keychain app",
                items.len()
            )
        }
        _ => "an unknown keychain error occurred".to_owned(),
    };
    GitYasError::Auth(format!("Could not {} the {provider} token: {detail}.{}", op.verb(), backend_hint()))
}

fn backend_hint() -> &'static str {
    if cfg!(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd")) {
        " Make sure a Secret Service keyring (for example GNOME Keyring or KWallet) is running and unlocked. \
         GitYas does not store tokens in plain text."
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use secrecy::{ExposeSecret, SecretString};

    use super::*;

    const TOKEN: &str = "ghp_SuperSecretValue123";

    fn cred(username: Option<&str>, token: &str) -> Credential {
        Credential {
            username: username.map(str::to_owned),
            token: SecretString::from(token),
        }
    }

    #[test]
    fn production_service_and_accounts() {
        let store = KeyringStore::new();
        assert_eq!(store.service(), "com.gityas.GitYas");
        assert_eq!(KeyringStore::default().service(), KEYCHAIN_SERVICE);
        let accounts: Vec<_> = Provider::ALL.into_iter().map(keychain_account).collect();
        assert_eq!(accounts, ["pat/github", "pat/gitlab", "pat/bitbucket", "pat/azure_devops"]);
    }

    #[test]
    fn supported_desktop_platforms_have_a_persistent_backend() {
        if cfg!(any(target_os = "macos", target_os = "windows", target_os = "linux")) {
            assert!(
                KeyringStore::new().is_backend_persistent(),
                "keyring features do not select a persistent backend"
            );
        }
    }

    #[test]
    fn non_persistent_backend_is_refused() {
        let store = KeyringStore {
            service: "test".into(),
            persistent: false,
            cache: Mutex::default(),
        };
        let err = store.set(Provider::Github, cred(None, TOKEN)).unwrap_err();
        assert!(matches!(&err, GitYasError::Auth(m) if m.contains("plain text")), "{err:?}");
        assert!(matches!(store.try_credential(Provider::Github), Err(GitYasError::Auth(_))));
        assert!(matches!(store.delete(Provider::Github), Err(GitYasError::Auth(_))));
        assert!(store.credential(Provider::Github).is_none());
        assert!(store.status().iter().all(|(_, c)| !c));
    }

    #[test]
    fn reads_are_cached_for_the_launch() {
        // The backend refuses every keychain call, so a cached answer proves the keychain wasn't asked again.
        let store = KeyringStore {
            service: "test".into(),
            persistent: false,
            cache: Mutex::default(),
        };
        store.cache().insert(Provider::Github, Some(cred(None, TOKEN)));
        store.cache().insert(Provider::Gitlab, None);
        let got = store.try_credential(Provider::Github).unwrap().unwrap();
        assert_eq!(got.token.expose_secret(), TOKEN);
        assert!(store.try_credential(Provider::Gitlab).unwrap().is_none());
        // Not cached yet: goes to the (refusing) keychain, and the error isn't cached.
        assert!(store.try_credential(Provider::Bitbucket).is_err());
        assert!(!store.cache().contains_key(&Provider::Bitbucket));
    }

    #[test]
    fn validation_runs_before_touching_the_keychain() {
        let store = KeyringStore {
            service: "test".into(),
            persistent: false,
            cache: Mutex::default(),
        };
        assert!(matches!(
            store.set(Provider::Github, cred(None, "  ")),
            Err(GitYasError::InvalidInput(_))
        ));
    }

    #[test]
    fn error_mapping_is_auth_and_leaks_nothing() {
        let platform = std::io::Error::other("org.freedesktop.DBus.Error.ServiceUnknown");
        let cases = [
            keyring::Error::PlatformFailure(Box::new(platform)),
            keyring::Error::NoStorageAccess(Box::new(std::io::Error::other("locked"))),
            keyring::Error::BadEncoding(TOKEN.as_bytes().to_vec()),
            keyring::Error::TooLong("password".into(), 10),
            keyring::Error::Invalid("service".into(), "empty".into()),
            keyring::Error::Ambiguous(Vec::new()),
        ];
        for e in &cases {
            let mapped = map_keyring_error(Op::Read, Provider::Github, e);
            let GitYasError::Auth(msg) = &mapped else {
                panic!("expected Auth, got {mapped:?}")
            };
            assert!(msg.starts_with("Could not read the GitHub token"), "{msg}");
            assert!(!msg.contains(TOKEN), "{msg}");
        }
        let GitYasError::Auth(msg) = map_keyring_error(Op::Save, Provider::Gitlab, &cases[0]) else {
            unreachable!()
        };
        assert!(msg.contains("ServiceUnknown") && msg.contains("save the GitLab token"), "{msg}");
    }

    #[test]
    fn debug_shows_no_secret() {
        let shown = format!("{:?}", KeyringStore::new());
        assert!(shown.contains("com.gityas.GitYas"));
    }

    /// Real keychain round trip under a throwaway service name (never the production one).
    /// Run with `cargo test -p gityas-auth -- --ignored keychain_smoke`. May show an OS keychain prompt.
    #[test]
    #[ignore = "touches the real OS keychain"]
    fn keychain_smoke() {
        let store = KeyringStore::with_service("com.gityas.GitYas.test-smoke");
        assert_ne!(store.service(), KEYCHAIN_SERVICE);
        let p = Provider::Bitbucket;

        store.delete(p).unwrap();
        assert!(store.try_credential(p).unwrap().is_none());

        store.set(p, cred(Some("alice"), &format!(" {TOKEN} "))).unwrap();
        let got = store.try_credential(p).unwrap().unwrap();
        assert_eq!(got.username.as_deref(), Some("alice"));
        assert_eq!(got.token.expose_secret(), TOKEN);
        assert!(store.status().contains(&(p, true)));

        store.set(p, cred(None, "replacement")).unwrap();
        let got = store.credential(p).unwrap();
        assert_eq!(got.username, None);
        assert_eq!(got.token.expose_secret(), "replacement");

        // A corrupt value written by something else reads as "not connected".
        keyring::Entry::new(store.service(), &keychain_account(p))
            .unwrap()
            .set_password("not json")
            .unwrap();
        assert!(store.try_credential(p).unwrap().is_none());

        store.delete(p).unwrap();
        store.delete(p).unwrap();
        assert!(store.credential(p).is_none());
        assert!(store.status().contains(&(p, false)));
    }
}
