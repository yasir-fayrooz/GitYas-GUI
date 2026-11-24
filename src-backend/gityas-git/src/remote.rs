//! Credential callback (attempt cap, `allowed_types`, PAT then git's saved credentials), fetch / push / merge /
//! abort merge, `detect_provider(remote_url)`, `test_connection`.
//!
//! Every network operation takes a [`RemoteContext`]: the credential source, the repository's resolved provider
//! (or `None` to detect it from each remote URL), an optional cancellation flag and an optional progress sink.

use std::sync::atomic::{AtomicBool, Ordering};

use gityas_core::{Credential, CredentialProvider, Provider};

mod auth;
mod gix_transfer;
mod merge_flow;
mod transfer;

pub(crate) use auth::{ConfigSource, Session};
pub use auth::{MAX_CREDENTIAL_ATTEMPTS, MAX_SSH_ATTEMPTS, default_username, detect_provider, is_ssh_url, redact_url_credentials};
pub(crate) use gix_transfer::{clone as clone_with_gitoxide, use_gitoxide};
pub use merge_flow::{AbortMergeOutcome, MergeOutcome, PullOutcome, abort_merge, merge_branch, pull, upstream_refname};
pub(crate) use merge_flow::{conflict_paths as conflict_paths_of, overwrite_error as merge_flow_overwrite_error};
pub(crate) use transfer::set_upstream_config as set_upstream;
pub use transfer::{
    FetchSummary, PushOutcome, RefUpdate, RemoteFetch, default_remote, fetch, fetch_remote, list_remote_refs, push_branch, push_refspecs,
    test_connection, test_connection_url,
};

/// Progress reported during network operations. Sideband text is the server's human-readable progress output
/// (decoded lossily: display only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferProgress {
    Fetch {
        received_objects: usize,
        total_objects: usize,
        indexed_objects: usize,
        indexed_deltas: usize,
        total_deltas: usize,
        received_bytes: usize,
    },
    Push {
        current: usize,
        total: usize,
        bytes: usize,
    },
    Sideband(String),
}

/// Inputs shared by every authenticated remote operation.
#[derive(Clone, Copy)]
pub struct RemoteContext<'a> {
    pub credentials: &'a dyn CredentialProvider,
    /// The repository's provider (stored on the repository). `None`: detect from each remote URL's host.
    pub provider: Option<Provider>,
    /// Fall back to git's saved credentials when no PAT is stored (or it is rejected): the configured credential
    /// helper (`credential.helper`) for HTTPS, ssh-agent and `~/.ssh` keys for SSH. Default true.
    pub use_credential_helper: bool,
    /// Set to `true` from another thread to abort; the operation then fails with `GitYasError::Cancelled`.
    pub cancel: Option<&'a AtomicBool>,
    pub progress: Option<&'a dyn Fn(&TransferProgress)>,
}

impl<'a> RemoteContext<'a> {
    pub fn new(credentials: &'a dyn CredentialProvider) -> Self {
        Self {
            credentials,
            provider: None,
            use_credential_helper: true,
            cancel: None,
            progress: None,
        }
    }

    pub fn with_provider(mut self, provider: Option<Provider>) -> Self {
        self.provider = provider;
        self
    }

    pub fn with_credential_helper(mut self, enabled: bool) -> Self {
        self.use_credential_helper = enabled;
        self
    }

    pub fn with_cancel(mut self, cancel: &'a AtomicBool) -> Self {
        self.cancel = Some(cancel);
        self
    }

    pub fn with_progress(mut self, progress: &'a dyn Fn(&TransferProgress)) -> Self {
        self.progress = Some(progress);
        self
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancel.is_some_and(|c| c.load(Ordering::SeqCst))
    }
}

impl std::fmt::Debug for RemoteContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteContext")
            .field("provider", &self.provider)
            .field("use_credential_helper", &self.use_credential_helper)
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// A credential source with no stored tokens (local remotes, tests, "helper only").
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCredentials;

impl CredentialProvider for NoCredentials {
    fn credential(&self, _provider: Provider) -> Option<Credential> {
        None
    }
}
