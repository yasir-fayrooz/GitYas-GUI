//! Personal-access-token storage. Tokens live only in the OS keychain, never in SQLite or logs.
//!
//! Keychain layout: service [`KEYCHAIN_SERVICE`], one item per provider, account `pat/<provider key>`,
//! value = JSON `{"username": …, "token": …}` (one item per provider means one OS prompt, not two).
//!
//! * [`KeyringStore`]: the real store (macOS Keychain, Windows Credential Manager, Secret Service on Linux/BSD).
//!   No plaintext fallback: when no persistent keychain is available every operation fails with `GitYasError::Auth`.
//! * [`MemoryCredentialStore`]: in-memory store for tests and `--mock` runs.
//! * [`provider_help`] / [`effective_username`]: per-provider username rules and UI help text.
//! * [`normalize_credential`]: the validation both stores apply on [`CredentialStore::set`].

mod codec;
mod help;
mod keyring_store;
mod memory;
mod validate;

use gityas_core::{Credential, CredentialProvider, Provider, Result};

pub use help::{ProviderHelp, effective_username, provider_help};
pub use keyring_store::KeyringStore;
pub use memory::MemoryCredentialStore;
pub use validate::{MAX_TOKEN_LEN, MAX_USERNAME_LEN, normalize_credential};

pub const KEYCHAIN_SERVICE: &str = "com.gityas.GitYas";

pub fn keychain_account(provider: Provider) -> String {
    format!("pat/{}", provider.key())
}

/// Read/write credential storage. "Connected" means a credential exists for the provider.
pub trait CredentialStore: CredentialProvider {
    /// Validates and normalises the credential ([`normalize_credential`]), then stores it, replacing any previous one.
    fn set(&self, provider: Provider, credential: Credential) -> Result<()>;
    /// Removes the provider's credential. Idempotent: deleting a missing credential succeeds.
    fn delete(&self, provider: Provider) -> Result<()>;
    /// Connection state for every provider in [`Provider::ALL`] order. Backend errors count as "not connected"
    /// (and are logged); use [`CredentialStore::try_credential`] to surface them.
    fn status(&self) -> Vec<(Provider, bool)>;

    /// Fallible lookup. `Ok(None)` = not connected (no entry, or an unreadable/corrupt entry, which is logged);
    /// `Err(GitYasError::Auth)` = the keychain itself could not be used (locked, no backend, access denied).
    /// [`CredentialProvider::credential`] is this with errors logged and mapped to `None`.
    fn try_credential(&self, provider: Provider) -> Result<Option<Credential>> {
        Ok(self.credential(provider))
    }
}
