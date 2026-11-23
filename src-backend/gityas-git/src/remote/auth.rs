//! Credential callback and provider detection.
//!
//! The decision logic lives in [`CredentialState`], a small state machine with no libgit2 dependency so it can be
//! unit-tested directly; [`Session::callbacks`] wires it into `git2::RemoteCallbacks`.
//!
//! Rules (fixing the old callback, where a rejected token looped forever and `allowed_types` was ignored):
//! * HTTPS: a PAT is offered only when libgit2 allows `USER_PASS_PLAINTEXT`. Candidates are tried in order, each at
//!   most once: the PAT linked in Settings -> Credentials, then git's own saved login through the configured
//!   credential helper (`git2::Cred::credential_helper`, never the CLI). At most [`MAX_CREDENTIAL_ATTEMPTS`].
//! * SSH (`ssh://`, `git+ssh://`, scp-like `git@host:path`): PATs are never sent. Candidates, each at most once: the
//!   ssh-agent, then the default keys in `~/.ssh` that have no passphrase (libgit2 aborts instead of moving on when a
//!   key can't be loaded). At most [`MAX_SSH_ATTEMPTS`]. Host keys are checked against `~/.ssh/known_hosts` by libgit2.
//! * Once the candidates run out the callback returns an `Auth` error, so libgit2 stops re-invoking it.
//! * The git-saved fallbacks (helper, agent, keys) are skipped when [`RemoteContext::use_credential_helper`] is off.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use git2::{CredentialType, ErrorClass, ErrorCode, RemoteCallbacks};
use gityas_core::{Credential, GitYasError, Provider, Result};
use secrecy::{ExposeSecret, SecretString};

use super::{RemoteContext, TransferProgress};

/// HTTPS credentials offered per connection before giving up.
pub const MAX_CREDENTIAL_ATTEMPTS: u32 = 2;

/// SSH credentials offered per connection before giving up (the agent plus each default key).
pub const MAX_SSH_ATTEMPTS: u32 = 1 + DEFAULT_SSH_KEYS.len() as u32;

/// `~/.ssh` key files tried after the agent, in OpenSSH's default order.
const DEFAULT_SSH_KEYS: [&str; 3] = ["id_ed25519", "id_ecdsa", "id_rsa"];

/// Username for SSH remotes whose URL has none (`ssh://host/repo`).
const SSH_DEFAULT_USER: &str = "git";

const SSH_HOST_UNTRUSTED: &str = "the remote's SSH host key isn't trusted: it is missing from ~/.ssh/known_hosts or has \
     changed. Connect once with ssh from a terminal to check and accept it, or use the HTTPS URL";

/// Replaces the user-info part (`user:token@`) of every `scheme://` URL in `text` with `***@`, so a remote URL with
/// embedded credentials never reaches an error message or a log line. Text without such URLs is returned unchanged.
pub fn redact_url_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("://") {
        let (head, tail) = rest.split_at(pos + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| matches!(c, '/' | '?' | '#' | '"' | '\'' | '`' | ')' | '>' | ']') || c.is_whitespace())
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***");
                out.push_str(&authority[at..]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// [`redact_url_credentials`] applied to the message of a user-facing error.
pub(crate) fn redact_error(err: GitYasError) -> GitYasError {
    match err {
        GitYasError::Git(m) => GitYasError::Git(redact_url_credentials(&m)),
        GitYasError::Auth(m) => GitYasError::Auth(redact_url_credentials(&m)),
        GitYasError::InvalidInput(m) => GitYasError::InvalidInput(redact_url_credentials(&m)),
        GitYasError::PushRejected(m) => GitYasError::PushRejected(redact_url_credentials(&m)),
        GitYasError::NotFound(m) => GitYasError::NotFound(redact_url_credentials(&m)),
        GitYasError::Other(m) => GitYasError::Other(redact_url_credentials(&m)),
        other => other,
    }
}

/// Is this an SSH remote URL (`ssh://`, `git+ssh://`, `ssh+git://` or scp-like `user@host:path`)?
pub fn is_ssh_url(url: &str) -> bool {
    let url = url.trim();
    if let Some((scheme, _)) = url.split_once("://") {
        return matches!(scheme.to_ascii_lowercase().as_str(), "ssh" | "git+ssh" | "ssh+git");
    }
    scp_host(url).is_some()
}

/// `user@host:path` -> `host`. Not a Windows drive (`C:\x`) and no `/` before the colon (git's rule).
fn scp_host(url: &str) -> Option<&str> {
    let (before, _) = url.split_once(':')?;
    if before.contains('/') || before.contains('\\') || before.len() <= 1 {
        return None;
    }
    let host = before.rsplit_once('@').map_or(before, |(_, h)| h);
    (!host.is_empty()).then_some(host)
}

/// Lower-cased host of an `https://`, `http://`, `ssh://` or scp-like URL.
fn url_host(url: &str) -> Option<String> {
    let url = url.trim();
    let host = if let Some((_, rest)) = url.split_once("://") {
        let authority = rest.split(['/', '?', '#']).next()?;
        let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        if let Some(v6) = host_port.strip_prefix('[') {
            v6.split(']').next()?
        } else {
            host_port.split(':').next()?
        }
    } else {
        scp_host(url)?
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// Hosted provider for a remote URL (https, ssh and scp-like forms). Self-hosted instances return `None`.
pub fn detect_provider(remote_url: &str) -> Option<Provider> {
    let host = url_host(remote_url)?;
    match host.as_str() {
        "github.com" | "www.github.com" | "ssh.github.com" => Some(Provider::Github),
        "gitlab.com" | "www.gitlab.com" | "altssh.gitlab.com" => Some(Provider::Gitlab),
        "bitbucket.org" | "www.bitbucket.org" | "altssh.bitbucket.org" => Some(Provider::Bitbucket),
        "dev.azure.com" | "ssh.dev.azure.com" => Some(Provider::AzureDevops),
        h if h == "visualstudio.com" || h.ends_with(".visualstudio.com") => Some(Provider::AzureDevops),
        _ => None,
    }
}

/// Username sent with a PAT when neither the stored credential nor the URL carries one.
/// Mirrors `gityas_auth::effective_username` defaults (gityas-git can't depend on gityas-auth); normally the
/// engine passes the resolved username in `Credential::username`.
pub fn default_username(provider: Provider) -> &'static str {
    match provider {
        Provider::Github => "x-access-token",
        Provider::Gitlab => "oauth2",
        Provider::Bitbucket => "x-bitbucket-api-token-auth",
        Provider::AzureDevops => "pat",
    }
}

/// A default SSH key found in `~/.ssh`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshKey {
    pub(crate) private: PathBuf,
    /// `<private>.pub`, when it exists.
    pub(crate) public: Option<PathBuf>,
    /// Passphrase-protected: never offered (it can't be unlocked without a prompt), only named in the error.
    pub(crate) encrypted: bool,
}

impl SshKey {
    fn name(&self) -> String {
        self.private
            .file_name()
            .map_or_else(|| self.private.display().to_string(), |n| n.to_string_lossy().into_owned())
    }
}

/// The default keys (`id_ed25519`, `id_ecdsa`, `id_rsa`) present in `~/.ssh`.
pub(crate) fn default_ssh_keys() -> Vec<SshKey> {
    let Some(home) = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()))
    else {
        return Vec::new();
    };
    ssh_keys_in(&PathBuf::from(home).join(".ssh"))
}

fn ssh_keys_in(dir: &Path) -> Vec<SshKey> {
    DEFAULT_SSH_KEYS
        .iter()
        .filter_map(|name| {
            let private = dir.join(name);
            let contents = std::fs::read_to_string(&private).ok()?;
            let public = dir.join(format!("{name}.pub"));
            Some(SshKey {
                encrypted: key_is_encrypted(&contents),
                public: public.is_file().then_some(public),
                private,
            })
        })
        .collect()
}

/// Whether a private key file needs a passphrase. OpenSSH format: the cipher name after the `openssh-key-v1` magic
/// is not `none`. PEM: a `Proc-Type: 4,ENCRYPTED` header or an `ENCRYPTED PRIVATE KEY` block. Unknown formats count
/// as encrypted, so they are skipped rather than aborting the connection.
fn key_is_encrypted(contents: &str) -> bool {
    use base64::Engine as _;
    const OPENSSH_BEGIN: &str = "-----BEGIN OPENSSH PRIVATE KEY-----";
    const MAGIC: &[u8] = b"openssh-key-v1\0";
    if let Some(start) = contents.find(OPENSSH_BEGIN) {
        let body: String = contents[start + OPENSSH_BEGIN.len()..]
            .lines()
            .take_while(|l| !l.starts_with("-----END"))
            .flat_map(|l| l.split_whitespace())
            .collect();
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(body) else {
            return true;
        };
        let Some(rest) = bytes.strip_prefix(MAGIC) else {
            return true;
        };
        let Some((len, rest)) = rest.split_first_chunk::<4>() else {
            return true;
        };
        let len = u32::from_be_bytes(*len) as usize;
        return rest.get(..len) != Some(b"none".as_slice());
    }
    if contents.contains("-----BEGIN ") && contents.contains(" PRIVATE KEY-----") {
        return contents.contains("ENCRYPTED");
    }
    true
}

/// What the callback should hand to libgit2 next.
#[derive(Debug)]
pub(crate) enum CredentialStep {
    Token {
        username: String,
        token: SecretString,
    },
    Helper {
        username: Option<String>,
    },
    /// SSH: libgit2 asks for the username first when the URL has none.
    SshUsername(String),
    SshAgent {
        username: String,
    },
    SshKey {
        username: String,
        key: SshKey,
    },
}

/// Per-connection credential decisions (see module docs).
#[derive(Debug, Clone)]
pub(crate) struct CredentialState {
    provider: Option<Provider>,
    stored: Option<Credential>,
    /// Use git's saved credentials (credential helper, ssh-agent, `~/.ssh` keys) after the stored PAT.
    helper_allowed: bool,
    attempts: u32,
    tried_token: bool,
    tried_helper: bool,
    helper_error: Option<String>,
    /// Default `~/.ssh` keys not offered yet (see [`default_ssh_keys`]).
    ssh_keys: Vec<SshKey>,
    ssh_attempts: u32,
    sent_ssh_username: bool,
    tried_agent: bool,
    /// The SSH candidates offered so far, for the final error.
    ssh_tried: Vec<String>,
}

impl CredentialState {
    pub(crate) fn new(provider: Option<Provider>, stored: Option<Credential>, helper_allowed: bool) -> Self {
        Self {
            provider,
            stored,
            helper_allowed,
            attempts: 0,
            tried_token: false,
            tried_helper: false,
            helper_error: None,
            ssh_keys: Vec::new(),
            ssh_attempts: 0,
            sent_ssh_username: false,
            tried_agent: false,
            ssh_tried: Vec::new(),
        }
    }

    pub(crate) fn with_ssh_keys(mut self, keys: Vec<SshKey>) -> Self {
        self.ssh_keys = keys;
        self
    }

    #[cfg(test)]
    pub(crate) fn attempts(&self) -> u32 {
        self.attempts
    }

    /// The helper produced nothing (or failed); the next call moves on.
    pub(crate) fn helper_failed(&mut self, reason: &str) {
        self.helper_error = Some(reason.to_owned());
    }

    /// Next credential to offer, or the final user-facing error message.
    pub(crate) fn next(&mut self, url: &str, username_from_url: Option<&str>, allowed: CredentialType) -> Result<CredentialStep, String> {
        let ssh_types = CredentialType::SSH_KEY
            | CredentialType::SSH_MEMORY
            | CredentialType::SSH_CUSTOM
            | CredentialType::SSH_INTERACTIVE
            | CredentialType::USERNAME;
        if is_ssh_url(url) || (!allowed.contains(CredentialType::USER_PASS_PLAINTEXT) && allowed.intersects(ssh_types)) {
            return self.next_ssh(url, username_from_url, allowed);
        }
        if !allowed.contains(CredentialType::USER_PASS_PLAINTEXT) {
            return Err(format!(
                "{} requested an authentication method GitYas does not support (only HTTPS tokens are supported)",
                redact_url_credentials(url)
            ));
        }
        if self.attempts >= MAX_CREDENTIAL_ATTEMPTS {
            return Err(self.exhausted_message(url));
        }
        if !self.tried_token {
            self.tried_token = true;
            if let Some(cred) = &self.stored {
                self.attempts += 1;
                let username = cred
                    .username
                    .as_deref()
                    .map(str::trim)
                    .filter(|u| !u.is_empty())
                    .or(username_from_url.filter(|u| !u.is_empty()))
                    .map(str::to_owned)
                    .or_else(|| self.provider.map(|p| default_username(p).to_owned()))
                    .unwrap_or_else(|| "git".to_owned());
                // `stored` is only ever looked up for the provider resolved for *this* URL (never a default
                // provider), so a token can't leak to another host.
                return Ok(CredentialStep::Token {
                    username,
                    token: cred.token.clone(),
                });
            }
        }
        if !self.tried_helper && self.helper_allowed {
            self.tried_helper = true;
            self.attempts += 1;
            return Ok(CredentialStep::Helper {
                username: username_from_url.map(str::to_owned),
            });
        }
        Err(self.exhausted_message(url))
    }

    /// SSH: the username, then the agent, then each unencrypted default key. A stored PAT is never sent.
    fn next_ssh(&mut self, url: &str, username_from_url: Option<&str>, allowed: CredentialType) -> Result<CredentialStep, String> {
        let username = username_from_url.filter(|u| !u.is_empty()).unwrap_or(SSH_DEFAULT_USER).to_owned();
        if !allowed.contains(CredentialType::SSH_KEY) {
            if allowed.contains(CredentialType::USERNAME) && !self.sent_ssh_username {
                self.sent_ssh_username = true;
                return Ok(CredentialStep::SshUsername(username));
            }
            return Err(format!(
                "{} requested an SSH authentication method GitYas does not support (only keys from ssh-agent or ~/.ssh)",
                redact_url_credentials(url)
            ));
        }
        if self.helper_allowed && self.ssh_attempts < MAX_SSH_ATTEMPTS {
            if !self.tried_agent {
                self.tried_agent = true;
                self.ssh_attempts += 1;
                self.ssh_tried.push("ssh-agent".to_owned());
                return Ok(CredentialStep::SshAgent { username });
            }
            if let Some(pos) = self.ssh_keys.iter().position(|k| !k.encrypted) {
                let key = self.ssh_keys.remove(pos);
                self.ssh_attempts += 1;
                self.ssh_tried.push(format!("~/.ssh/{}", key.name()));
                return Ok(CredentialStep::SshKey { username, key });
            }
        }
        Err(self.ssh_exhausted_message(url))
    }

    fn ssh_exhausted_message(&self, url: &str) -> String {
        let url = redact_url_credentials(url);
        if !self.helper_allowed {
            return format!("{url} is an SSH remote; GitYas signs in to it only with ssh-agent or keys in ~/.ssh");
        }
        let locked: Vec<String> = self
            .ssh_keys
            .iter()
            .filter(|k| k.encrypted)
            .map(|k| format!("~/.ssh/{}", k.name()))
            .collect();
        let hint = if locked.is_empty() {
            "add your key to ssh-agent and to your account on the server, or use the HTTPS URL".to_owned()
        } else {
            format!(
                "{} {} a passphrase; add it to ssh-agent (ssh-add) so GitYas can use it",
                locked.join(", "),
                if locked.len() == 1 { "has" } else { "have" }
            )
        };
        format!("SSH sign-in to {url} failed (tried {}); {hint}", self.ssh_tried.join(", "))
    }

    fn exhausted_message(&self, url: &str) -> String {
        let url = redact_url_credentials(url);
        let provider = self
            .provider
            .map_or_else(|| "this remote's provider".to_owned(), |p| p.display_name().to_owned());
        if self.stored.is_some() {
            format!("the {provider} personal access token was rejected by {url}; check that it is valid and has the required scopes")
        } else if self.tried_helper && self.helper_error.is_none() {
            format!(
                "the login saved by git (credential helper) was rejected by {url}; sign in again with git, or add a \
                 personal access token for {provider} in Settings"
            )
        } else if self.provider.is_none() {
            format!(
                "{url} requires authentication; set the repository's provider and add a personal access token, or save a \
                 login with git's credential helper"
            )
        } else {
            format!(
                "{url} requires authentication; add a personal access token for {provider} in Settings, or save a login \
                 with git's credential helper"
            )
        }
    }
}

/// Where the credential helper reads `credential.*` config from.
#[derive(Debug, Clone)]
pub(crate) enum ConfigSource {
    /// Global/XDG/system config (clone, detached remotes).
    Default,
    /// Default config plus the repository's own `config` file (path of the git dir).
    Repo(PathBuf),
}

impl ConfigSource {
    fn open(&self) -> Result<git2::Config, git2::Error> {
        let mut config = git2::Config::open_default()?;
        if let ConfigSource::Repo(git_dir) = self {
            let local = git_dir.join("config");
            if local.exists() {
                config.add_file(&local, git2::ConfigLevel::Local, false)?;
            }
        }
        Ok(config)
    }
}

#[derive(Debug, Default)]
pub(crate) struct SessionLog {
    /// Last final credential failure (preferred over libgit2's generic message).
    pub(crate) auth_failure: Option<String>,
    /// `(refname, status)` for every ref the remote rejected during a push.
    pub(crate) rejected: Vec<(String, String)>,
}

/// One remote operation: resolves the provider and stored PAT once, hands out fresh callbacks per connection,
/// and maps libgit2 errors to user-facing ones.
pub(crate) struct Session<'a> {
    ctx: &'a RemoteContext<'a>,
    template: CredentialState,
    config: ConfigSource,
    log: Rc<RefCell<SessionLog>>,
}

impl<'a> Session<'a> {
    pub(crate) fn new(ctx: &'a RemoteContext<'a>, url: &str, config: ConfigSource) -> Result<Self> {
        let provider = ctx.provider.or_else(|| detect_provider(url));
        let template = if is_ssh_url(url) {
            // A PAT is never sent over SSH, so the keychain isn't read for it.
            let keys = if ctx.use_credential_helper {
                default_ssh_keys()
            } else {
                Vec::new()
            };
            CredentialState::new(provider, None, ctx.use_credential_helper).with_ssh_keys(keys)
        } else {
            let stored = provider.and_then(|p| ctx.credentials.credential(p));
            CredentialState::new(provider, stored, ctx.use_credential_helper)
        };
        Ok(Self {
            ctx,
            template,
            config,
            log: Rc::new(RefCell::new(SessionLog::default())),
        })
    }

    pub(crate) fn check_cancelled(&self) -> Result<()> {
        if self.ctx.is_cancelled() {
            Err(GitYasError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Takes the rejected push refs recorded so far.
    pub(crate) fn take_rejected(&self) -> Vec<(String, String)> {
        std::mem::take(&mut self.log.borrow_mut().rejected)
    }

    pub(crate) fn callbacks(&self) -> RemoteCallbacks<'a> {
        let mut cbs = RemoteCallbacks::new();
        let cancel = self.ctx.cancel;
        let progress = self.ctx.progress;

        let mut credential = CredentialCallback {
            state: self.template.clone(),
            config: self.config.clone(),
            log: Rc::clone(&self.log),
            cancel,
        };
        cbs.credentials(move |url, username_from_url, allowed| credential.call(url, username_from_url, allowed));

        cbs.transfer_progress(move |p| {
            if let Some(report) = progress {
                report(&TransferProgress::Fetch {
                    received_objects: p.received_objects(),
                    total_objects: p.total_objects(),
                    indexed_objects: p.indexed_objects(),
                    indexed_deltas: p.indexed_deltas(),
                    total_deltas: p.total_deltas(),
                    received_bytes: p.received_bytes(),
                });
            }
            !cancelled(cancel)
        });
        cbs.sideband_progress(move |bytes| {
            if let Some(report) = progress {
                report(&TransferProgress::Sideband(String::from_utf8_lossy(bytes).into_owned()));
            }
            !cancelled(cancel)
        });
        cbs.push_transfer_progress(move |current, total, bytes| {
            if let Some(report) = progress {
                report(&TransferProgress::Push { current, total, bytes });
            }
        });
        cbs.push_negotiation(move |_| {
            if cancelled(cancel) {
                Err(git2::Error::new(ErrorCode::User, ErrorClass::Callback, "operation cancelled"))
            } else {
                Ok(())
            }
        });
        let log = Rc::clone(&self.log);
        cbs.push_update_reference(move |refname, status| {
            if let Some(status) = status {
                log.borrow_mut().rejected.push((refname.to_owned(), status.to_owned()));
            }
            Ok(())
        });
        cbs
    }

    /// libgit2 error -> user-facing error. Cancellation wins; then our own credential failure message.
    pub(crate) fn map_error(&self, err: git2::Error) -> GitYasError {
        if self.ctx.is_cancelled() {
            return GitYasError::Cancelled;
        }
        if let Some(message) = self.log.borrow().auth_failure.clone() {
            return GitYasError::Auth(message);
        }
        // libgit2 messages can quote the remote URL, which may carry `user:token@`.
        redact_error(match (err.code(), err.class()) {
            (ErrorCode::Auth, _) => GitYasError::Auth(err.message().to_owned()),
            (ErrorCode::Certificate, ErrorClass::Ssh) => GitYasError::Auth(SSH_HOST_UNTRUSTED.to_owned()),
            (_, ErrorClass::Ssh) => GitYasError::Auth(format!("SSH: {}", err.message())),
            (ErrorCode::NotFastForward, _) => GitYasError::PushRejected(
                "the remote branch has commits you don't have locally; pull (fetch and merge) first, or force push".to_owned(),
            ),
            _ => crate::git_error(err),
        })
    }
}

/// The libgit2 credentials callback: drives [`CredentialState`] and turns its steps into `git2::Cred`s.
pub(crate) struct CredentialCallback<'a> {
    state: CredentialState,
    config: ConfigSource,
    log: Rc<RefCell<SessionLog>>,
    cancel: Option<&'a AtomicBool>,
}

impl CredentialCallback<'_> {
    pub(crate) fn call(&mut self, url: &str, username_from_url: Option<&str>, allowed: CredentialType) -> Result<git2::Cred, git2::Error> {
        if cancelled(self.cancel) {
            return Err(git2::Error::new(ErrorCode::User, ErrorClass::Callback, "operation cancelled"));
        }
        loop {
            match self.state.next(url, username_from_url, allowed) {
                Ok(CredentialStep::Token { username, token }) => {
                    return git2::Cred::userpass_plaintext(&username, token.expose_secret());
                }
                Ok(CredentialStep::SshUsername(username)) => return git2::Cred::username(&username),
                Ok(CredentialStep::SshAgent { username }) => return git2::Cred::ssh_key_from_agent(&username),
                Ok(CredentialStep::SshKey { username, key }) => {
                    return git2::Cred::ssh_key(&username, key.public.as_deref(), &key.private, None);
                }
                Ok(CredentialStep::Helper { username }) => {
                    match self
                        .config
                        .open()
                        .and_then(|c| git2::Cred::credential_helper(&c, url, username.as_deref()))
                    {
                        Ok(cred) => return Ok(cred),
                        Err(e) => {
                            tracing::debug!(error = e.message(), "git credential helper provided no credentials");
                            self.state.helper_failed(e.message());
                        }
                    }
                }
                Err(message) => {
                    self.log.borrow_mut().auth_failure = Some(message.clone());
                    return Err(git2::Error::new(ErrorCode::Auth, ErrorClass::Callback, message));
                }
            }
        }
    }
}

fn cancelled(flag: Option<&AtomicBool>) -> bool {
    flag.is_some_and(|c| c.load(Ordering::SeqCst))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_credentials_are_redacted() {
        assert_eq!(
            redact_url_credentials("failed to connect to https://user:ghp_secret@github.com/o/r.git: 401"),
            "failed to connect to https://***@github.com/o/r.git: 401"
        );
        assert_eq!(
            redact_url_credentials("'http://tok@h:8080' and https://a:b@c@d/x"),
            "'http://***@h:8080' and https://***@d/x"
        );
        for clean in [
            "https://github.com/o/r.git",
            "no url here",
            "git@github.com:o/r.git",
            "file:///tmp/a@b",
            "ends ://",
        ] {
            assert_eq!(redact_url_credentials(clean), clean);
        }
        assert_eq!(
            redact_error(GitYasError::Auth("rejected by https://x:y@h/r".into())),
            GitYasError::Auth("rejected by https://***@h/r".into())
        );
        let mut state = CredentialState::new(Some(Provider::Github), Some(pat(None)), false);
        let _ = state.next("https://u:secret@github.com/o/r", None, CredentialType::USER_PASS_PLAINTEXT);
        let message = state
            .next("https://u:secret@github.com/o/r", None, CredentialType::USER_PASS_PLAINTEXT)
            .err()
            .unwrap_or_default();
        assert!(
            message.contains("https://***@github.com/o/r") && !message.contains("secret"),
            "{message}"
        );
    }

    fn pat(username: Option<&str>) -> Credential {
        Credential {
            username: username.map(str::to_owned),
            token: SecretString::from("tok"),
        }
    }

    const HTTPS: &str = "https://github.com/o/r.git";

    #[test]
    fn ssh_detection() {
        for url in [
            "ssh://git@github.com/o/r.git",
            "SSH://host/x",
            "git+ssh://host/x",
            "git@github.com:o/r.git",
            "host.xz:path/to/repo",
        ] {
            assert!(is_ssh_url(url), "{url}");
        }
        for url in [
            "https://github.com/o/r.git",
            "http://h/x",
            "file:///tmp/r",
            "/tmp/r",
            "C:\\code\\r",
            "C:/code/r",
            "./a:b",
        ] {
            assert!(!is_ssh_url(url), "{url}");
        }
    }

    #[test]
    fn provider_detection() {
        for (url, p) in [
            ("https://github.com/o/r.git", Some(Provider::Github)),
            ("https://user:pw@GitHub.com:443/o/r", Some(Provider::Github)),
            ("git@github.com:o/r.git", Some(Provider::Github)),
            ("ssh://git@ssh.github.com:443/o/r.git", Some(Provider::Github)),
            ("https://gitlab.com/g/sub/r.git", Some(Provider::Gitlab)),
            ("git@gitlab.com:g/r.git", Some(Provider::Gitlab)),
            ("https://me@bitbucket.org/ws/r.git", Some(Provider::Bitbucket)),
            ("git@bitbucket.org:ws/r.git", Some(Provider::Bitbucket)),
            ("https://org@dev.azure.com/org/p/_git/r", Some(Provider::AzureDevops)),
            ("git@ssh.dev.azure.com:v3/org/p/r", Some(Provider::AzureDevops)),
            ("https://org.visualstudio.com/p/_git/r", Some(Provider::AzureDevops)),
            ("org@vs-ssh.visualstudio.com:v3/org/p/r", Some(Provider::AzureDevops)),
            ("https://github.com.evil.example/o/r", None),
            ("https://notgithub.com/o/r", None),
            ("https://gitlab.example.com/o/r", None),
            ("file:///tmp/r.git", None),
            ("/tmp/r.git", None),
            ("", None),
            ("https://[::1]:8080/r", None),
        ] {
            assert_eq!(detect_provider(url), p, "{url}");
        }
    }

    #[test]
    fn token_only_with_plaintext_allowed() {
        let mut st = CredentialState::new(Some(Provider::Github), Some(pat(None)), false);
        assert!(st.next(HTTPS, None, CredentialType::DEFAULT).is_err());
        assert_eq!(st.attempts(), 0);
        let mut st = CredentialState::new(Some(Provider::Github), Some(pat(None)), false);
        let err = st
            .next(HTTPS, None, CredentialType::SSH_KEY | CredentialType::USERNAME)
            .unwrap_err();
        assert!(err.contains("SSH"), "{err}");
    }

    #[test]
    fn ssh_url_never_gets_a_token() {
        let mut st = CredentialState::new(Some(Provider::Github), Some(pat(None)), true);
        let err = st
            .next("git@github.com:o/r.git", Some("git"), CredentialType::USER_PASS_PLAINTEXT)
            .unwrap_err();
        assert!(err.contains("SSH"));
    }

    fn step_name(step: &Result<CredentialStep, String>) -> String {
        match step {
            Ok(CredentialStep::Token { .. }) => "token".into(),
            Ok(CredentialStep::Helper { .. }) => "helper".into(),
            Ok(CredentialStep::SshUsername(u)) => format!("user:{u}"),
            Ok(CredentialStep::SshAgent { .. }) => "agent".into(),
            Ok(CredentialStep::SshKey { key, .. }) => key.name(),
            Err(_) => "error".into(),
        }
    }

    /// Every step offered for an HTTPS remote until the state machine gives up.
    fn https_steps(stored: bool, helper: bool) -> Vec<String> {
        let mut st = CredentialState::new(Some(Provider::Github), stored.then(|| pat(None)), helper);
        let mut steps = Vec::new();
        loop {
            let step = st.next(HTTPS, None, CredentialType::USER_PASS_PLAINTEXT);
            steps.push(step_name(&step));
            if step.is_err() {
                return steps;
            }
        }
    }

    /// A linked PAT goes first; git's saved login (credential helper) is the fallback, and the only candidate when
    /// no PAT is linked.
    #[test]
    fn https_resolution_order() {
        assert_eq!(https_steps(true, true), ["token", "helper", "error"]);
        assert_eq!(https_steps(false, true), ["helper", "error"]);
        assert_eq!(https_steps(true, false), ["token", "error"]);
        assert_eq!(https_steps(false, false), ["error"]);
    }

    #[test]
    fn rejected_git_login_message() {
        let mut st = CredentialState::new(Some(Provider::Github), None, true);
        assert!(matches!(
            st.next(HTTPS, None, CredentialType::USER_PASS_PLAINTEXT),
            Ok(CredentialStep::Helper { .. })
        ));
        let err = st.next(HTTPS, None, CredentialType::USER_PASS_PLAINTEXT).unwrap_err();
        assert!(err.contains("saved by git (credential helper) was rejected"), "{err}");
    }

    fn key(name: &str, encrypted: bool) -> SshKey {
        SshKey {
            private: PathBuf::from("/home/me/.ssh").join(name),
            public: None,
            encrypted,
        }
    }

    const SSH: &str = "git@github.com:o/r.git";
    const SSH_AUTH: CredentialType = CredentialType::SSH_KEY.union(CredentialType::SSH_CUSTOM);

    /// SSH: the username when the URL has none, then ssh-agent, then the default keys without a passphrase. A stored
    /// PAT is never offered, and the passphrase-protected key is named in the error.
    #[test]
    fn ssh_resolution_order() {
        let keys = vec![key("id_ed25519", true), key("id_ecdsa", false), key("id_rsa", false)];
        let mut st = CredentialState::new(Some(Provider::Github), Some(pat(None)), true).with_ssh_keys(keys);
        assert_eq!(
            step_name(&st.next("ssh://github.com/o/r.git", None, CredentialType::USERNAME)),
            "user:git"
        );
        // A second username request (libgit2 never does this) must not loop.
        assert_eq!(
            step_name(&st.next("ssh://github.com/o/r.git", None, CredentialType::USERNAME)),
            "error"
        );
        let mut steps = Vec::new();
        let err = loop {
            match st.next(SSH, Some("git"), SSH_AUTH) {
                Err(e) => break e,
                step => steps.push(step_name(&step)),
            }
            assert!(steps.len() as u32 <= MAX_SSH_ATTEMPTS, "callback kept offering SSH credentials");
        };
        assert_eq!(steps, ["agent", "id_ecdsa", "id_rsa"]);
        assert!(err.contains("tried ssh-agent, ~/.ssh/id_ecdsa, ~/.ssh/id_rsa"), "{err}");
        assert!(err.contains("~/.ssh/id_ed25519 has a passphrase"), "{err}");
        assert!(st.next(SSH, Some("git"), SSH_AUTH).is_err(), "stays failed");

        let mut st = CredentialState::new(None, None, true);
        assert_eq!(step_name(&st.next(SSH, Some("me"), SSH_AUTH)), "agent");
        let err = st.next(SSH, Some("me"), SSH_AUTH).unwrap_err();
        assert!(err.contains("add your key to ssh-agent"), "{err}");
    }

    #[test]
    fn ssh_without_git_fallbacks_fails_at_once() {
        let mut st = CredentialState::new(Some(Provider::Github), Some(pat(None)), false).with_ssh_keys(vec![key("id_rsa", false)]);
        let err = st.next(SSH, Some("git"), SSH_AUTH).unwrap_err();
        assert!(err.contains("SSH remote"), "{err}");
        let err = st.next(SSH, Some("git"), CredentialType::SSH_INTERACTIVE).unwrap_err();
        assert!(err.contains("does not support"), "{err}");
    }

    /// The real callback against an SSH remote that rejects everything: it answers the username, then each SSH
    /// candidate once, then stops with an `Auth` error.
    #[test]
    fn ssh_callback_against_always_rejecting_fake_remote_terminates() {
        let log = Rc::new(RefCell::new(SessionLog::default()));
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(dir.path().join("id_rsa"), openssh_key("none")).unwrap_or_else(|e| panic!("{e}"));
        let mut cb = CredentialCallback {
            state: CredentialState::new(Some(Provider::Github), Some(pat(None)), true).with_ssh_keys(ssh_keys_in(dir.path())),
            config: ConfigSource::Default,
            log: Rc::clone(&log),
            cancel: None,
        };
        let user = cb
            .call("ssh://github.com/o/r", None, CredentialType::USERNAME)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(user.credtype() as u32, CredentialType::USERNAME.bits());
        let mut sent = 0;
        let err = loop {
            match cb.call(SSH, Some("git"), SSH_AUTH) {
                Ok(cred) => {
                    assert_eq!(cred.credtype() as u32, CredentialType::SSH_KEY.bits());
                    sent += 1;
                    assert!(sent <= MAX_SSH_ATTEMPTS, "callback keeps answering a rejecting remote");
                }
                Err(e) => break e,
            }
        };
        assert_eq!(sent, 2, "agent + id_rsa");
        assert_eq!(err.code(), ErrorCode::Auth);
        assert!(log.borrow().auth_failure.as_deref().is_some_and(|m| m.contains("SSH sign-in")));
    }

    /// An OpenSSH private key file whose body starts with the magic and the given cipher name (the rest is not
    /// needed to tell whether it is encrypted).
    fn openssh_key(cipher: &str) -> String {
        use base64::Engine as _;
        let mut bytes = b"openssh-key-v1\0".to_vec();
        bytes.extend_from_slice(&(cipher.len() as u32).to_be_bytes());
        bytes.extend_from_slice(cipher.as_bytes());
        bytes.extend_from_slice(&[0; 40]);
        let body = base64::engine::general_purpose::STANDARD.encode(bytes);
        let (a, b) = body.split_at(20);
        format!("-----BEGIN OPENSSH PRIVATE KEY-----\n{a}\n{b}\n-----END OPENSSH PRIVATE KEY-----\n")
    }

    #[test]
    fn encrypted_keys_are_detected() {
        assert!(!key_is_encrypted(&openssh_key("none")));
        assert!(key_is_encrypted(&openssh_key("aes256-ctr")));
        assert!(!key_is_encrypted(&openssh_key("none").replace('\n', "\r\n")));
        assert!(!key_is_encrypted(
            "-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----\n"
        ));
        assert!(key_is_encrypted(
            "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,00\n\nMIIE\n-----END RSA PRIVATE KEY-----\n"
        ));
        assert!(key_is_encrypted(
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nMIIE\n-----END ENCRYPTED PRIVATE KEY-----\n"
        ));
        assert!(key_is_encrypted(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n!!!\n-----END OPENSSH PRIVATE KEY-----\n"
        ));
        assert!(key_is_encrypted("not a key"));
    }

    #[test]
    fn default_keys_are_found_in_order() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let write = |name: &str, contents: &str| std::fs::write(dir.path().join(name), contents).unwrap_or_else(|e| panic!("{e}"));
        write("id_rsa", &openssh_key("none"));
        write("id_ed25519", &openssh_key("aes256-ctr"));
        write("id_ed25519.pub", "ssh-ed25519 AAAA me");
        write("id_dsa", &openssh_key("none"));
        let keys = ssh_keys_in(dir.path());
        assert_eq!(keys.iter().map(SshKey::name).collect::<Vec<_>>(), ["id_ed25519", "id_rsa"]);
        assert!(keys[0].encrypted && keys[0].public.as_deref() == Some(dir.path().join("id_ed25519.pub").as_path()));
        assert!(!keys[1].encrypted && keys[1].public.is_none());
    }

    #[test]
    fn username_precedence() {
        let take = |st: &mut CredentialState, url_user| match st.next(HTTPS, url_user, CredentialType::USER_PASS_PLAINTEXT) {
            Ok(CredentialStep::Token { username, token }) => {
                assert_eq!(token.expose_secret(), "tok");
                username
            }
            other => panic!("expected token, got {other:?}"),
        };
        let mut st = CredentialState::new(Some(Provider::Gitlab), Some(pat(Some("me"))), false);
        assert_eq!(take(&mut st, Some("url-user")), "me");
        let mut st = CredentialState::new(Some(Provider::Gitlab), Some(pat(Some("  "))), false);
        assert_eq!(take(&mut st, Some("url-user")), "url-user");
        let mut st = CredentialState::new(Some(Provider::Bitbucket), Some(pat(None)), false);
        assert_eq!(take(&mut st, None), "x-bitbucket-api-token-auth");
        let mut st = CredentialState::new(None, Some(pat(None)), false);
        assert_eq!(take(&mut st, None), "git");
    }

    /// A remote that rejects every credential: the state machine must terminate.
    #[test]
    fn always_rejecting_remote_terminates() {
        for (stored, helper) in [(true, true), (true, false), (false, true), (false, false)] {
            let mut st = CredentialState::new(Some(Provider::Github), stored.then(|| pat(None)), helper);
            let mut offered = 0;
            let err = loop {
                match st.next(HTTPS, None, CredentialType::USER_PASS_PLAINTEXT) {
                    Ok(_) => offered += 1,
                    Err(e) => break e,
                }
                assert!(offered <= MAX_CREDENTIAL_ATTEMPTS, "callback kept offering credentials");
            };
            assert_eq!(offered, u32::from(stored) + u32::from(helper));
            assert!(!err.is_empty());
            if stored {
                assert!(err.contains("token was rejected"), "{err}");
            }
            // Further calls keep failing instead of resetting.
            assert!(st.next(HTTPS, None, CredentialType::USER_PASS_PLAINTEXT).is_err());
        }
    }

    /// The real callback against a fake remote that rejects whatever it is given (libgit2 re-invokes the
    /// callback after each 401): it must stop with an `Auth` error after at most the cap.
    #[test]
    fn callback_against_always_rejecting_fake_remote_terminates() {
        let log = Rc::new(RefCell::new(SessionLog::default()));
        let mut cb = CredentialCallback {
            state: CredentialState::new(Some(Provider::Github), Some(pat(None)), false),
            config: ConfigSource::Default,
            log: Rc::clone(&log),
            cancel: None,
        };
        let mut creds_sent = 0;
        let err = loop {
            match cb.call(HTTPS, None, CredentialType::USER_PASS_PLAINTEXT) {
                Ok(cred) => {
                    assert!(cred.has_username());
                    creds_sent += 1;
                    assert!(creds_sent <= MAX_CREDENTIAL_ATTEMPTS, "callback keeps answering a rejecting remote");
                }
                Err(e) => break e,
            }
        };
        assert_eq!(creds_sent, 1);
        assert_eq!(err.code(), ErrorCode::Auth);
        assert!(
            log.borrow()
                .auth_failure
                .as_deref()
                .is_some_and(|m| m.contains("token was rejected"))
        );

        let cancel = AtomicBool::new(true);
        let mut cb = CredentialCallback {
            state: CredentialState::new(Some(Provider::Github), Some(pat(None)), false),
            config: ConfigSource::Default,
            log,
            cancel: Some(&cancel),
        };
        assert_eq!(
            cb.call(HTTPS, None, CredentialType::USER_PASS_PLAINTEXT).err().map(|e| e.code()),
            Some(ErrorCode::User)
        );
    }

    #[test]
    fn helper_failure_message() {
        let mut st = CredentialState::new(Some(Provider::Github), None, true);
        assert!(matches!(
            st.next(HTTPS, None, CredentialType::USER_PASS_PLAINTEXT),
            Ok(CredentialStep::Helper { .. })
        ));
        st.helper_failed("nothing");
        let err = st.next(HTTPS, None, CredentialType::USER_PASS_PLAINTEXT).unwrap_err();
        assert!(err.contains("add a personal access token for GitHub"), "{err}");
    }

    #[test]
    fn token_is_not_in_debug_output() {
        let st = CredentialState::new(Some(Provider::Github), Some(pat(None)), true);
        assert!(!format!("{st:?}").contains("tok\""));
    }
}
