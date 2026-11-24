//! fetch / push / connection test / remote ref listing.

use std::collections::BTreeMap;

use git2::{Direction, ErrorCode, FetchPrune, Repository};
use gityas_core::{GitYasError, Result};

use super::RemoteContext;
use super::auth::{ConfigSource, Session};
use crate::convert::oid_from_git;
use crate::git_error;
use crate::repository::remote_names;

/// A ref changed by a fetch (`old`/`new` `None` = created/deleted). Non-UTF-8 ref names are shown lossily.
pub use gityas_core::models::{FetchSummary, PushOutcome, RefUpdate, RemoteFetch};

/// Fetches `remote`, or every remote when `None`, with the remotes' configured refspecs.
/// `prune` removes remote-tracking refs deleted on the remote (otherwise `remote.<name>.prune` decides).
///
/// With `None`: no remotes -> empty summary; a failing remote doesn't stop the others; if every remote fails
/// (or the operation is cancelled) the first error is returned.
pub fn fetch(repo: &Repository, remote: Option<&str>, prune: bool, ctx: &RemoteContext<'_>) -> Result<FetchSummary> {
    let names = match remote {
        Some(name) => vec![name.to_owned()],
        None => remote_names(repo)?,
    };
    let mut summary = FetchSummary::default();
    for name in names {
        match fetch_remote(repo, &name, prune, ctx) {
            Ok(f) => summary.fetched.push(f),
            Err(GitYasError::Cancelled) => return Err(GitYasError::Cancelled),
            Err(e) if remote.is_some() => return Err(e),
            Err(e) => {
                tracing::warn!(remote = %name, error = %e, "fetch failed");
                summary.failed.push((name, e));
            }
        }
    }
    if summary.fetched.is_empty()
        && let Some((_, first)) = summary.failed.first()
    {
        return Err(first.clone());
    }
    Ok(summary)
}

pub fn fetch_remote(repo: &Repository, name: &str, prune: bool, ctx: &RemoteContext<'_>) -> Result<RemoteFetch> {
    let mut remote = find_remote(repo, name)?;
    let url = remote
        .url()
        .ok_or_else(|| GitYasError::InvalidInput(format!("the URL of remote {name:?} is not valid UTF-8")))?
        .to_owned();
    let session = Session::new(ctx, &url, ConfigSource::Repo(repo.path().to_path_buf()))?;
    session.check_cancelled()?;

    // gitoxide unless the transport's program (ssh, or git for local paths) is missing.
    let repo_dir = repo.workdir().unwrap_or_else(|| repo.path());
    let gix_repo = crate::gitoxide::open(repo_dir).ok();
    if gix_repo.is_some() && super::gix_transfer::use_gitoxide(&url, gix_repo.as_ref()) {
        return super::gix_transfer::fetch_remote(repo_dir, name, &url, prune, ctx, &session);
    }

    let before = ref_snapshot(repo)?;
    let mut opts = git2::FetchOptions::new();
    opts.remote_callbacks(session.callbacks());
    opts.prune(if prune { FetchPrune::On } else { FetchPrune::Unspecified });
    // No `update_tips` callback on purpose: git2 `unwrap`s ref names as UTF-8 there; diff snapshots instead.
    remote.fetch::<&str>(&[], Some(&mut opts), None).map_err(|e| session.map_error(e))?;
    let after = ref_snapshot(repo)?;
    Ok(RemoteFetch {
        remote: name.to_owned(),
        updated: diff_snapshots(&before, &after)?,
    })
}

type Snapshot = BTreeMap<Vec<u8>, Option<git2::Oid>>;

fn ref_snapshot(repo: &Repository) -> Result<Snapshot> {
    let mut map = Snapshot::new();
    for r in repo.references().map_err(git_error)? {
        let r = r.map_err(git_error)?;
        map.insert(r.name_bytes().to_vec(), r.target());
    }
    Ok(map)
}

fn diff_snapshots(before: &Snapshot, after: &Snapshot) -> Result<Vec<RefUpdate>> {
    let opt = |o: Option<&Option<git2::Oid>>| o.copied().flatten().map(oid_from_git).transpose();
    let mut out = Vec::new();
    for name in before.keys().chain(after.keys().filter(|k| !before.contains_key(*k))) {
        let (old, new) = (before.get(name), after.get(name));
        if old != new {
            out.push(RefUpdate {
                refname: String::from_utf8_lossy(name).into_owned(),
                old: opt(old)?,
                new: opt(new)?,
            });
        }
    }
    Ok(out)
}

pub(crate) fn find_remote<'r>(repo: &'r Repository, name: &str) -> Result<git2::Remote<'r>> {
    repo.find_remote(name).map_err(|e| match e.code() {
        ErrorCode::NotFound => GitYasError::NotFound(format!("remote {name:?} does not exist")),
        ErrorCode::Invalid => GitYasError::InvalidInput(format!("invalid remote name {name:?}")),
        _ => git_error(e),
    })
}

/// The remote to push to when a branch has no upstream: `origin` if present, else the only remote.
pub fn default_remote(repo: &Repository) -> Result<String> {
    let names = remote_names(repo)?;
    if names.iter().any(|n| n == "origin") {
        return Ok("origin".to_owned());
    }
    match names.as_slice() {
        [only] => Ok(only.clone()),
        [] => Err(GitYasError::InvalidInput(
            "this repository has no remotes; add one first".to_owned(),
        )),
        _ => Err(GitYasError::InvalidInput(
            "the branch has no upstream and there are several remotes; choose one (create remote branch)".to_owned(),
        )),
    }
}

/// Pushes raw refspecs to `remote` (push URL if configured). Fails if the remote rejects any ref (reported via
/// `push_update_reference`) or libgit2 refuses a non-fast-forward update.
pub fn push_refspecs(repo: &Repository, remote: &str, refspecs: &[String], ctx: &RemoteContext<'_>) -> Result<()> {
    let mut git_remote = find_remote(repo, remote)?;
    let url = git_remote
        .pushurl()
        .or(git_remote.url())
        .ok_or_else(|| GitYasError::InvalidInput(format!("the URL of remote {remote:?} is not valid UTF-8")))?
        .to_owned();
    let session = Session::new(ctx, &url, ConfigSource::Repo(repo.path().to_path_buf()))?;
    session.check_cancelled()?;
    let mut opts = git2::PushOptions::new();
    opts.remote_callbacks(session.callbacks());
    git_remote.push(refspecs, Some(&mut opts)).map_err(|e| session.map_error(e))?;
    let rejected = session.take_rejected();
    if !rejected.is_empty() {
        let detail = rejected.iter().map(|(r, s)| format!("{r} ({s})")).collect::<Vec<_>>().join(", ");
        // Pulling first only helps when every rejection is about history the remote has and we don't.
        let behind = |status: &str| {
            let s = status.to_ascii_lowercase();
            s.contains("non-fast-forward") || s.contains("fetch first")
        };
        return Err(if rejected.iter().all(|(_, s)| behind(s)) {
            GitYasError::PushRejected(detail)
        } else {
            GitYasError::Git(format!("the remote rejected the push: {detail}"))
        });
    }
    Ok(())
}

/// Pushes local branch `local_refname` (`refs/heads/x`) to its upstream. Without an upstream it pushes to the
/// same name on [`default_remote`] and records that as the upstream. `force` uses a `+` refspec.
///
/// Unlike the old `push` command, tags are **not** pushed implicitly (use `refs::push_tag`).
pub fn push_branch(repo: &Repository, local_refname: &str, force: bool, ctx: &RemoteContext<'_>) -> Result<PushOutcome> {
    let short = local_refname
        .strip_prefix("refs/heads/")
        .ok_or_else(|| GitYasError::InvalidInput(format!("{local_refname:?} is not a local branch")))?;
    match repo.find_reference(local_refname) {
        Ok(_) => {}
        Err(e) if e.code() == ErrorCode::NotFound => {
            return Err(GitYasError::NotFound(format!(
                "branch {short:?} does not exist or has no commits yet"
            )));
        }
        Err(e) => return Err(git_error(e)),
    }
    let configured = configured_upstream(repo, local_refname)?;
    let (remote, remote_ref, upstream_set) = match configured {
        Some((remote, merge)) => (remote, merge, false),
        None => (default_remote(repo)?, local_refname.to_owned(), true),
    };
    let spec = format!("{}{local_refname}:{remote_ref}", if force { "+" } else { "" });
    push_refspecs(repo, &remote, &[spec], ctx)?;
    if upstream_set {
        set_upstream_config(repo, short, &remote, &remote_ref)?;
    }
    Ok(PushOutcome {
        remote,
        local_ref: local_refname.to_owned(),
        remote_ref,
        upstream_set,
    })
}

/// `(branch.<x>.remote, branch.<x>.merge)` if both are configured (even if the tracking ref is gone).
pub(crate) fn configured_upstream(repo: &Repository, local_refname: &str) -> Result<Option<(String, String)>> {
    let remote = match repo.branch_upstream_remote(local_refname) {
        Ok(buf) => buf,
        Err(e) if e.code() == ErrorCode::NotFound => return Ok(None),
        Err(e) => return Err(git_error(e)),
    };
    let merge = match repo.branch_upstream_merge(local_refname) {
        Ok(buf) => buf,
        Err(e) if e.code() == ErrorCode::NotFound => return Ok(None),
        Err(e) => return Err(git_error(e)),
    };
    let remote = remote
        .as_str()
        .ok_or_else(|| GitYasError::InvalidInput("upstream remote name is not valid UTF-8".to_owned()))?;
    let merge = merge
        .as_str()
        .ok_or_else(|| GitYasError::InvalidInput("upstream branch name is not valid UTF-8".to_owned()))?;
    if remote == "." {
        // Tracking a local branch: nothing to push to.
        return Ok(None);
    }
    Ok(Some((remote.to_owned(), merge.to_owned())))
}

/// Writes `branch.<short>.remote/merge` directly (works even before the remote-tracking ref exists).
pub(crate) fn set_upstream_config(repo: &Repository, short: &str, remote: &str, merge_ref: &str) -> Result<()> {
    let mut config = repo.config().map_err(git_error)?;
    config.set_str(&format!("branch.{short}.remote"), remote).map_err(git_error)?;
    config.set_str(&format!("branch.{short}.merge"), merge_ref).map_err(git_error)?;
    Ok(())
}

/// Connects to `remote` for fetch, proving the URL and credentials work (Settings → "Test connection"). The smart
/// transports authenticate and read the ref advertisement during the connect itself, so the refs are not listed:
/// `git2::RemoteConnection::list` hits a debug-mode abort (null slice) on a remote with no refs (git2 0.20.4).
pub fn test_connection(repo: &Repository, remote: &str, ctx: &RemoteContext<'_>) -> Result<()> {
    let mut git_remote = find_remote(repo, remote)?;
    let url = git_remote
        .url()
        .ok_or_else(|| GitYasError::InvalidInput(format!("the URL of remote {remote:?} is not valid UTF-8")))?
        .to_owned();
    let session = Session::new(ctx, &url, ConfigSource::Repo(repo.path().to_path_buf()))?;
    session.check_cancelled()?;
    git_remote
        .connect_auth(Direction::Fetch, Some(session.callbacks()), None)
        .map_err(|e| session.map_error(e))?;
    Ok(())
}

/// Same as [`test_connection`] for a URL that isn't configured as a remote (e.g. before cloning).
pub fn test_connection_url(url: &str, ctx: &RemoteContext<'_>) -> Result<()> {
    let mut remote = git2::Remote::create_detached(url).map_err(git_error)?;
    let session = Session::new(ctx, url, ConfigSource::Default)?;
    session.check_cancelled()?;
    remote
        .connect_auth(Direction::Fetch, Some(session.callbacks()), None)
        .map_err(|e| session.map_error(e))?;
    Ok(())
}

/// Ref names advertised by `remote` (peeled `^{}` entries removed, sorted, deduplicated).
pub fn list_remote_refs(repo: &Repository, remote: &str, ctx: &RemoteContext<'_>) -> Result<Vec<String>> {
    let mut git_remote = find_remote(repo, remote)?;
    let url = git_remote
        .url()
        .ok_or_else(|| GitYasError::InvalidInput(format!("the URL of remote {remote:?} is not valid UTF-8")))?
        .to_owned();
    let session = Session::new(ctx, &url, ConfigSource::Repo(repo.path().to_path_buf()))?;
    session.check_cancelled()?;
    let local = local_remote_path(&url);
    if local.is_some_and(local_repository_has_no_refs) {
        return Ok(Vec::new());
    }
    let conn = git_remote
        .connect_auth(Direction::Fetch, Some(session.callbacks()), None)
        .map_err(|e| session.map_error(e))?;
    // Network remotes: `default_branch` (safe: implemented in C over the same advertisement) is `NotFound` when the
    // advertisement is empty or lacks HEAD. Only call `list()` when HEAD was advertised, i.e. the list is non-empty.
    // A remote whose HEAD is not advertised (dangling default branch) is reported as having no refs.
    if local.is_none() {
        match conn.default_branch() {
            Ok(_) => {}
            Err(e) if e.code() == ErrorCode::NotFound => {
                tracing::debug!(remote, "remote advertises no HEAD; treating it as having no refs");
                return Ok(Vec::new());
            }
            Err(e) => return Err(session.map_error(e)),
        }
    }
    let heads = conn.list().map_err(|e| session.map_error(e))?;
    let mut names = Vec::with_capacity(heads.len());
    for head in heads {
        // git2's `RemoteHead::name` unwraps the name as UTF-8 (no bytes accessor); contain that panic so one
        // odd remote ref can't take the app down.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| head.name().to_owned())) {
            Ok(name) if !name.ends_with("^{}") => names.push(name),
            Ok(_) => {}
            Err(_) => tracing::warn!(remote, "skipping a remote ref with a non-UTF-8 name"),
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// Workaround for git2 0.20 `RemoteConnection::list`, which builds its slice with `slice::from_raw_parts(base, size)`
/// while libgit2 returns a NULL `base` for an empty advertisement (e.g. a repository without refs). That is undefined
/// behaviour: debug builds abort the whole process ("unsafe precondition(s) violated"). [`list_remote_refs`] therefore
/// only calls `list()` once it knows the advertisement is non-empty: local remotes are checked directly (the local
/// transport advertises every ref), network remotes via `default_branch`.
fn local_remote_path(url: &str) -> Option<&str> {
    match url.strip_prefix("file://") {
        Some(path) => Some(strip_drive_slash(path)),
        None if !url.contains("://") && !super::is_ssh_url(url) => Some(url),
        None => None,
    }
}

/// `/C:/dir/repo.git` (from `file:///C:/dir/repo.git`) → `C:/dir/repo.git`; anything else unchanged.
fn strip_drive_slash(path: &str) -> &str {
    match path.as_bytes() {
        [b'/', drive, b':', ..] if drive.is_ascii_alphabetic() => &path[1..],
        _ => path,
    }
}

/// `true` only when the repository at `path` opens and has no refs at all (unreadable: `false`, let libgit2 report it).
fn local_repository_has_no_refs(path: &str) -> bool {
    match Repository::open(path) {
        Ok(remote_repo) => match remote_repo.references() {
            Ok(mut refs) => refs.next().is_none(),
            Err(_) => false,
        },
        Err(_) => false,
    }
}
