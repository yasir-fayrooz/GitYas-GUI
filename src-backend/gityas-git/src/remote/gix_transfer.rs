//! Fetch and clone through gitoxide. Push and everything that writes the working tree stay on libgit2.
//!
//! Transports: HTTPS through reqwest (native TLS, the OS certificate store); SSH through the system `ssh` program,
//! so `~/.ssh/config`, ssh-agent, every key type and `known_hosts` behave exactly as for the git CLI (gitoxide
//! starts it without a console window and with stdin closed: a prompt fails instead of hanging); local paths and
//! `file://` through `git upload-pack`. [`use_gitoxide`] falls back to libgit2 when the program a transport needs is
//! missing, so nothing that worked before stops working on a machine without OpenSSH or git.
//!
//! Authentication follows the same decisions and messages as the libgit2 path ([`CredentialState`]): the stored
//! personal access token, then git's saved login (the configured credential helpers, never prompting), each tried
//! once. gitoxide authenticates a connection with one credential, so each candidate is a new attempt.
//!
//! The transfer runs on a worker thread; the calling thread polls gitoxide's progress tree and forwards it to
//! [`RemoteContext::progress`] (which need not be `Send`), and the session's cancel flag interrupts it.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use git2::CredentialType;
use gityas_core::models::{RefUpdate, RemoteFetch};
use gityas_core::{GitYasError, Result};
use gix::bstr::ByteSlice;
use secrecy::{ExposeSecret, SecretString};

use super::auth::{CredentialStep, Session, is_ssh_url, redact_error, redact_url_credentials};
use super::{RemoteContext, TransferProgress};
use crate::gitoxide::{self, gix_error, oid_from_gix};
use crate::refscan::{RefScanCache, RefSnapshot, scan_refs};

/// How often the calling thread forwards progress while a transfer runs.
const POLL: Duration = Duration::from_millis(50);

/// Whether `url` goes through gitoxide: always for HTTP(S) and `git://`; for SSH when an `ssh` program is
/// configured or on `PATH`; for local paths when `git` is on `PATH` (gitoxide runs `git upload-pack` for them).
pub(crate) fn use_gitoxide(url: &str, config: Option<&gix::Repository>) -> bool {
    let Ok(parsed) = gix::url::parse(url.as_bytes().as_bstr()) else {
        return false;
    };
    match parsed.scheme {
        gix::url::Scheme::Http | gix::url::Scheme::Https | gix::url::Scheme::Git => true,
        gix::url::Scheme::Ssh => ssh_configured(config) || on_path("ssh"),
        gix::url::Scheme::File => on_path("git"),
        gix::url::Scheme::Ext | gix::url::Scheme::Helper(_) | gix::url::Scheme::HelperUrl(_) => false,
    }
}

fn ssh_configured(config: Option<&gix::Repository>) -> bool {
    ["GIT_SSH_COMMAND", "GIT_SSH"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|v| !v.is_empty()))
        || config.is_some_and(|repo| repo.config_snapshot().string("core.sshCommand").is_some())
}

/// `program` (with `.exe` on Windows) exists in a `PATH` directory.
fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{program}.exe"), format!("{program}.cmd"), program.to_owned()]
    } else {
        vec![program.to_owned()]
    };
    std::env::split_paths(&path).any(|dir| names.iter().any(|name| dir.join(name).is_file()))
}

/// One authentication candidate for an attempt.
#[derive(Clone)]
enum Creds {
    /// Provide nothing (public remotes, SSH, or nothing left to offer).
    Nothing,
    Token {
        username: String,
        token: SecretString,
    },
    /// git's configured credential helpers (no prompt).
    Helper,
}

impl From<CredentialStep> for Creds {
    fn from(step: CredentialStep) -> Self {
        match step {
            CredentialStep::Token { username, token } => Creds::Token { username, token },
            CredentialStep::Helper { .. } => Creds::Helper,
            _ => Creds::Nothing,
        }
    }
}

/// What the credentials callback saw during one attempt.
#[derive(Debug, Default)]
struct AuthLog {
    /// The server asked for credentials.
    asked: bool,
    /// The helper had no login (or failed): the reason, for [`CredentialState::helper_failed`].
    helper_empty: Option<String>,
}

enum AttemptError {
    /// The server required authentication and this attempt's credential did not satisfy it.
    Auth {
        helper_empty: Option<String>,
    },
    Other(GitYasError),
}

/// Runs `transfer` once per credential candidate until it succeeds, fails for another reason, or the candidates run
/// out (then the user-facing message of [`CredentialState`]).
fn with_credentials<T>(
    session: &Session<'_>,
    ctx: &RemoteContext<'_>,
    url: &str,
    mut transfer: impl FnMut(Creds) -> std::result::Result<T, AttemptError>,
) -> Result<T> {
    let mut state = session.credential_state();
    if is_ssh_url(url) {
        // The ssh program authenticates (agent, keys, ~/.ssh/config); the setting that turns git's saved
        // credentials off still means SSH cannot sign in.
        if !ctx.use_credential_helper {
            let message = state
                .next(url, None, CredentialType::SSH_KEY)
                .err()
                .unwrap_or_else(|| format!("{} is an SSH remote", redact_url_credentials(url)));
            return Err(GitYasError::Auth(message));
        }
        return transfer(Creds::Nothing).map_err(|err| match err {
            AttemptError::Auth { .. } => GitYasError::Auth(format!("SSH sign-in to {} failed", redact_url_credentials(url))),
            AttemptError::Other(err) => err,
        });
    }
    let mut tried = false;
    loop {
        let (creds, final_message) = match state.next(url, None, CredentialType::USER_PASS_PLAINTEXT) {
            Ok(step) => (Creds::from(step), None),
            Err(message) if !tried => (Creds::Nothing, Some(message)),
            Err(message) => return Err(redact_error(GitYasError::Auth(message))),
        };
        tried = true;
        match transfer(creds) {
            Ok(value) => return Ok(value),
            Err(AttemptError::Other(err)) => return Err(err),
            Err(AttemptError::Auth { helper_empty }) => {
                if let Some(reason) = helper_empty {
                    state.helper_failed(&reason);
                }
                if let Some(message) = final_message {
                    return Err(redact_error(GitYasError::Auth(message)));
                }
            }
        }
    }
}

type CredentialsFn = Box<dyn FnMut(gix::credentials::helper::Action) -> gix::Result<Option<gix::credentials::protocol::Outcome>> + Send>;

/// The gitoxide credentials callback for one attempt.
fn credentials_fn(repo: &gix::Repository, url: &str, creds: Creds, log: Arc<Mutex<AuthLog>>) -> CredentialsFn {
    use gix::credentials::helper::Action;
    let helpers = match &creds {
        Creds::Helper => gix::url::parse(url.as_bytes().as_bstr())
            .ok()
            .and_then(|url| repo.config_snapshot().credential_helpers(url).ok())
            .map(|(cascade, _, mut prompt)| {
                // Never prompt (no terminal; libgit2's helper did not prompt either).
                prompt.mode = gix::prompt::Mode::Disable;
                prompt.askpass = None;
                (cascade, prompt)
            }),
        _ => None,
    };
    let mut helpers = helpers;
    Box::new(move |action| {
        let record = |f: &dyn Fn(&mut AuthLog)| {
            if let Ok(mut log) = log.lock() {
                f(&mut log);
            }
        };
        match action {
            Action::Get(context) => {
                record(&|l| l.asked = true);
                match &creds {
                    Creds::Nothing => Ok(None),
                    Creds::Token { username, token } => Ok(Some(gix::credentials::protocol::Outcome {
                        credentials: gix::sec::identity::Credentials::UsernamePassword {
                            username: username.clone(),
                            password: token.expose_secret().to_owned(),
                            oauth_refresh_token: None,
                        },
                        next: context.into(),
                    })),
                    Creds::Helper => {
                        let outcome = match helpers.as_mut() {
                            Some((cascade, prompt)) => cascade.invoke(Action::Get(context), prompt.clone()),
                            None => Ok(None),
                        };
                        match outcome {
                            Ok(Some(found)) => Ok(Some(found)),
                            Ok(None) => {
                                record(&|l| l.helper_empty = Some("no saved login".into()));
                                Ok(None)
                            }
                            Err(err) => {
                                let reason = format!("{err:#}");
                                tracing::debug!(error = %reason, "git credential helper provided no credentials");
                                record(&|l| l.helper_empty = Some(reason.clone()));
                                Ok(None)
                            }
                        }
                    }
                }
            }
            // Like the libgit2 path: never store or erase in the user's helper.
            Action::Store(_) | Action::Erase(_) => Ok(None),
        }
    })
}

/// Classifies a failed attempt. `asked`: the server requested credentials during it.
fn classify(err: gix::Error, url: &str, log: &AuthLog, ctx: &RemoteContext<'_>) -> AttemptError {
    if ctx.is_cancelled() {
        return AttemptError::Other(GitYasError::Cancelled);
    }
    let message = format!("{err:#}");
    let lower = message.to_ascii_lowercase();
    if lower.contains("credentials in clear text") {
        // gitoxide never sends credentials over unencrypted http:// (libgit2 did); trying the next candidate is
        // pointless.
        return AttemptError::Other(GitYasError::Auth(format!(
            "{} requires signing in, and GitYas does not send credentials over an unencrypted http:// connection; \
             use the remote's https:// URL",
            redact_url_credentials(url)
        )));
    }
    if log.asked || lower.contains("authentication") || lower.contains("401") || lower.contains("permission denied (publickey") {
        return AttemptError::Auth {
            helper_empty: log.helper_empty.clone(),
        };
    }
    let err = if lower.contains("host key verification failed") {
        GitYasError::Auth(super::auth::SSH_HOST_UNTRUSTED.to_owned())
    } else if is_ssh_url(url) && (lower.contains("could not read from remote") || lower.contains("connection closed")) {
        GitYasError::Auth(format!("SSH: {message}"))
    } else {
        GitYasError::Git(message)
    };
    AttemptError::Other(redact_error(err))
}

/// Runs `work` on a worker thread while this thread forwards gitoxide progress to `ctx.progress`. A first
/// "counting objects" report goes out before the transfer starts; `None` when the operation was cancelled by then.
fn with_progress<T: Send>(ctx: &RemoteContext<'_>, work: impl FnOnce(&gix::progress::tree::Root) -> T + Send) -> Option<T> {
    if let Some(report) = ctx.progress {
        report(&TransferProgress::Fetch {
            received_objects: 0,
            total_objects: 0,
            indexed_objects: 0,
            indexed_deltas: 0,
            total_deltas: 0,
            received_bytes: 0,
        });
    }
    if ctx.is_cancelled() {
        return None;
    }
    let root = gix::progress::tree::Root::new();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| work(&root));
        let mut snapshot = Vec::new();
        let mut last: Option<TransferProgress> = None;
        loop {
            let finished = worker.is_finished();
            if let Some(report) = ctx.progress {
                root.sorted_snapshot(&mut snapshot);
                if let Some(progress) = progress_from(&snapshot)
                    && last.as_ref() != Some(&progress)
                {
                    report(&progress);
                    last = Some(progress);
                }
            }
            if finished {
                break;
            }
            std::thread::sleep(POLL);
        }
        match worker.join() {
            Ok(value) => Some(value),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

/// The final 100% report once a pack of `objects` objects was received and indexed (a fast transfer can finish
/// between two polls).
fn report_done(ctx: &RemoteContext<'_>, objects: usize) {
    if let Some(report) = ctx.progress
        && objects > 0
    {
        report(&TransferProgress::Fetch {
            received_objects: objects,
            total_objects: objects,
            indexed_objects: objects,
            indexed_deltas: 0,
            total_deltas: 0,
            received_bytes: 0,
        });
    }
}

/// Objects in the pack a fetch received (0 when none was needed).
fn pack_objects(outcome: &gix::remote::fetch::Outcome) -> usize {
    match &outcome.status {
        gix::remote::fetch::Status::Change { write_pack_bundle, .. } => write_pack_bundle.index.num_objects as usize,
        gix::remote::fetch::Status::NoPackReceived { .. } => 0,
    }
}

/// The libgit2-shaped progress report from gitoxide's progress tree: objects indexed of the pack total, deltas
/// resolved, bytes received; before the pack arrives, the server's own progress line.
fn progress_from(tasks: &[(gix::progress::prodash::progress::Key, gix::progress::Task)]) -> Option<TransferProgress> {
    let value = |id: &[u8; 4]| {
        tasks.iter().rev().find(|(_, t)| &t.id == id).and_then(|(_, t)| {
            let v = t.progress.as_ref()?;
            Some((v.step.load(std::sync::atomic::Ordering::Relaxed), v.done_at.unwrap_or(0)))
        })
    };
    if let Some((received, total)) = value(b"IWIO") {
        let (resolved, deltas) = value(b"IWRO").unwrap_or((0, 0));
        let (bytes, _) = value(b"BWRB").unwrap_or((0, 0));
        return Some(TransferProgress::Fetch {
            received_objects: received,
            total_objects: total,
            indexed_objects: received,
            indexed_deltas: resolved.min(deltas),
            total_deltas: deltas,
            received_bytes: bytes,
        });
    }
    let (_, remote) = tasks.iter().rev().find(|(_, t)| &t.id == b"FERP" && t.progress.is_some())?;
    let v = remote.progress.as_ref()?;
    let step = v.step.load(std::sync::atomic::Ordering::Relaxed);
    let line = match v.done_at {
        Some(total) if total > 0 => format!("{}: {}% ({step}/{total})", remote.name, step * 100 / total),
        _ => format!("{}: {step}", remote.name),
    };
    Some(TransferProgress::Sideband(line))
}

/// Fetches remote `name` of the repository at `repo_dir` with its configured refspecs (tags auto-followed), then
/// prunes its remote-tracking refs deleted on the remote when `prune` or `remote.<name>.prune` / `fetch.prune`.
pub(crate) fn fetch_remote(
    repo_dir: &Path,
    name: &str,
    url: &str,
    prune: bool,
    ctx: &RemoteContext<'_>,
    session: &Session<'_>,
) -> Result<RemoteFetch> {
    let repo = gitoxide::open(repo_dir)?;
    let mut scans = RefScanCache::default();
    let before = scan_refs(&repo, &mut scans)?;
    let never = AtomicBool::new(false);
    let interrupt = ctx.cancel.unwrap_or(&never);
    let shared = repo.clone().into_sync();

    let outcome = with_credentials(session, ctx, url, |creds| {
        let log = Arc::new(Mutex::new(AuthLog::default()));
        let result = with_progress(ctx, |root| -> gix::Result<(Vec<String>, usize)> {
            let repo = shared.to_thread_local();
            let remote = repo.find_remote(name.as_bytes().as_bstr())?;
            let mut connection = remote.connect(gix::remote::Direction::Fetch)?;
            connection.set_credentials(credentials_fn(&repo, url, creds, Arc::clone(&log)));
            let prepared = connection.prepare_fetch(root.add_child("handshake"), Default::default())?;
            let outcome = prepared.receive(root.add_child("fetch"), interrupt)?;
            // Destinations the remote still has (for pruning).
            let kept = outcome
                .ref_map
                .mappings
                .iter()
                .filter_map(|m| m.local.as_ref().map(|l| l.to_string()))
                .collect();
            Ok((kept, pack_objects(&outcome)))
        });
        let Some(result) = result else {
            return Err(AttemptError::Other(GitYasError::Cancelled));
        };
        result.map_err(|err| {
            let log = log
                .lock()
                .map(|l| AuthLog {
                    asked: l.asked,
                    helper_empty: l.helper_empty.clone(),
                })
                .unwrap_or_default();
            classify(err, url, &log, ctx)
        })
    })?;

    let (kept, objects) = outcome;
    report_done(ctx, objects);
    if prune || prune_configured(&repo, name) {
        let current = scan_refs(&repo, &mut scans)?;
        prune_tracking_refs(&repo, name, &current, &kept.into_iter().collect())?;
    }
    let after = scan_refs(&repo, &mut scans)?;
    Ok(RemoteFetch {
        remote: name.to_owned(),
        updated: diff(&before, &after)?,
    })
}

fn prune_configured(repo: &gix::Repository, remote: &str) -> bool {
    let config = repo.config_snapshot();
    let file = config.plumbing();
    let remote_prune = file.boolean_by("remote", Some(remote.into()), "prune").ok().flatten();
    remote_prune
        .or_else(|| file.boolean_by("fetch", None::<&gix::bstr::BStr>, "prune").ok().flatten())
        .unwrap_or(false)
}

/// Deletes the direct refs under remote `name`'s fetch refspec destinations (never under `refs/tags/`) that the
/// fetch no longer mapped from the remote: `git fetch --prune` without `--prune-tags`.
fn prune_tracking_refs(repo: &gix::Repository, name: &str, refs: &RefSnapshot, kept: &HashSet<String>) -> Result<()> {
    let remote = repo.find_remote(name.as_bytes().as_bstr()).map_err(gix_error)?;
    let destinations: Vec<String> = remote
        .refspecs(gix::remote::Direction::Fetch)
        .iter()
        .filter_map(|spec| match spec.to_ref().instruction() {
            gix::refspec::Instruction::Fetch(gix::refspec::instruction::Fetch::AndUpdate { dst, .. }) => Some(dst.to_string()),
            _ => None,
        })
        .filter(|dst| !dst.starts_with("refs/tags/"))
        .collect();
    let mut edits = Vec::new();
    for entry in &refs.refs {
        let Some(target) = entry.target else { continue };
        if kept.contains(&entry.name) || !destinations.iter().any(|dst| star_match(dst, &entry.name)) {
            continue;
        }
        edits.push(gix::refs::transaction::RefEdit {
            change: gix::refs::transaction::Change::Delete {
                expected: gix::refs::transaction::PreviousValue::MustExistAndMatch(gix::refs::Target::Object(target)),
                log: gix::refs::transaction::RefLog::AndReference,
            },
            name: entry.name.as_str().try_into().map_err(gix_error)?,
            deref: false,
        });
    }
    if !edits.is_empty() {
        tracing::debug!(remote = name, pruned = edits.len(), "pruning remote-tracking refs");
        repo.edit_references(edits).map_err(gix_error)?;
    }
    Ok(())
}

fn star_match(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((prefix, suffix)) => name.len() >= prefix.len() + suffix.len() && name.starts_with(prefix) && name.ends_with(suffix),
    }
}

/// Refs whose direct target changed between two scans (`old`/`new` `None` = created/deleted).
fn diff(before: &RefSnapshot, after: &RefSnapshot) -> Result<Vec<RefUpdate>> {
    let map = |s: &RefSnapshot| -> BTreeMap<String, Option<gix::ObjectId>> { s.refs.iter().map(|e| (e.name.clone(), e.target)).collect() };
    let (before, after) = (map(before), map(after));
    let opt = |o: Option<&Option<gix::ObjectId>>| o.copied().flatten().map(|id| oid_from_gix(&id)).transpose();
    let mut out = Vec::new();
    for name in before.keys().chain(after.keys().filter(|k| !before.contains_key(*k))) {
        let (old, new) = (before.get(name), after.get(name));
        if old != new {
            out.push(RefUpdate {
                refname: name.clone(),
                old: opt(old)?,
                new: opt(new)?,
            });
        }
    }
    Ok(out)
}

/// Clones `url` into `dest` (absent or empty) with gitoxide and checks out the remote's default branch. On failure
/// gitoxide removes what it created.
pub(crate) fn clone(url: &str, dest: &Path, ctx: &RemoteContext<'_>, session: &Session<'_>) -> Result<()> {
    let never = AtomicBool::new(false);
    let interrupt = ctx.cancel.unwrap_or(&never);
    with_credentials(session, ctx, url, |creds| {
        let log = Arc::new(Mutex::new(AuthLog::default()));
        let result = with_progress(ctx, |root| -> gix::Result<usize> {
            let callback_log = Arc::clone(&log);
            let callback_url = url.to_owned();
            let mut prepare = gix::prepare_clone(url, dest)?
                .with_in_memory_config_overrides(gitoxide::IDENTITY_FALLBACK)
                .configure_connection(move |connection| {
                    let repo = connection.remote().repo().clone();
                    connection.set_credentials(credentials_fn(&repo, &callback_url, creds.clone(), Arc::clone(&callback_log)));
                    Ok(())
                });
            let (mut checkout, fetched) = prepare.fetch_then_checkout(root.add_child("fetch"), interrupt)?;
            let _ = checkout.main_worktree(root.add_child("checkout"), interrupt)?;
            Ok(pack_objects(&fetched))
        });
        let Some(result) = result else {
            return Err(AttemptError::Other(GitYasError::Cancelled));
        };
        result.map(|objects| report_done(ctx, objects)).map_err(|err| {
            let log = log
                .lock()
                .map(|l| AuthLog {
                    asked: l.asked,
                    helper_empty: l.helper_empty.clone(),
                })
                .unwrap_or_default();
            classify(err, url, &log, ctx)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transports() {
        assert!(use_gitoxide("https://github.com/o/r.git", None));
        assert!(use_gitoxide("http://h/x", None));
        assert!(use_gitoxide("git://h/x", None));
        assert!(!use_gitoxide("ext::some command", None));
    }

    #[test]
    fn progress_prefers_pack_counts_over_remote_lines() {
        assert_eq!(progress_from(&[]), None);
    }

    #[test]
    fn star_patterns() {
        assert!(star_match("refs/remotes/origin/*", "refs/remotes/origin/a/b"));
        assert!(!star_match("refs/remotes/origin/*", "refs/remotes/other/a"));
        assert!(star_match("refs/remotes/origin/main", "refs/remotes/origin/main"));
    }
}
