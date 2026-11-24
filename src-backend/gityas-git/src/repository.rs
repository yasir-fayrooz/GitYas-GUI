//! Open / init / clone, repository-state queries (bare, shallow, worktree, merge in progress), remotes.
//!
//! Everything is path based and stateless: callers open a fresh `git2::Repository` per command (it is `!Sync`).

use std::path::{Path, PathBuf};

use git2::{ErrorCode, Repository, RepositoryInitOptions, RepositoryState};
use gityas_core::models::{CommitSignature, RepoOperation, RepoStatus};
use gityas_core::{GitYasError, Provider, Result};

use crate::git_error;
use crate::remote::{ConfigSource, RemoteContext, Session, detect_provider, is_ssh_url};

/// In-progress operation recorded in the git dir (`MERGE_HEAD`, `rebase-merge/`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepoState {
    Clean,
    Merge,
    Revert,
    CherryPick,
    Bisect,
    Rebase,
    ApplyMailbox,
}

impl RepoState {
    pub fn from_git(state: RepositoryState) -> Self {
        match state {
            RepositoryState::Clean => RepoState::Clean,
            RepositoryState::Merge => RepoState::Merge,
            RepositoryState::Revert | RepositoryState::RevertSequence => RepoState::Revert,
            RepositoryState::CherryPick | RepositoryState::CherryPickSequence => RepoState::CherryPick,
            RepositoryState::Bisect => RepoState::Bisect,
            RepositoryState::Rebase | RepositoryState::RebaseInteractive | RepositoryState::RebaseMerge => RepoState::Rebase,
            RepositoryState::ApplyMailbox | RepositoryState::ApplyMailboxOrRebase => RepoState::ApplyMailbox,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            RepoState::Clean => "nothing",
            RepoState::Merge => "a merge",
            RepoState::Revert => "a revert",
            RepoState::CherryPick => "a cherry-pick",
            RepoState::Bisect => "a bisect",
            RepoState::Rebase => "a rebase",
            RepoState::ApplyMailbox => "an `git am` session",
        }
    }
}

/// Snapshot of the repository-level facts the UI and engine need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryInfo {
    /// The `.git` directory (for a linked worktree: `.git/worktrees/<name>`; for a bare repo: the repo itself).
    pub git_dir: PathBuf,
    /// `None` for bare repositories.
    pub workdir: Option<PathBuf>,
    pub is_bare: bool,
    pub is_shallow: bool,
    /// True for a linked worktree (`git worktree add`), false for the main checkout.
    pub is_worktree: bool,
    /// No commits on HEAD yet.
    pub head_unborn: bool,
    pub head_detached: bool,
    pub state: RepoState,
}

/// A configured remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInfo {
    pub name: String,
    /// `None` if the configured URL is not valid UTF-8.
    pub url: Option<String>,
    pub push_url: Option<String>,
    /// Detected from the (push or fetch) URL host.
    pub provider: Option<Provider>,
    /// SSH remotes sign in with ssh-agent or `~/.ssh` keys, never with a personal access token.
    pub is_ssh: bool,
}

/// Opens the repository at `path` exactly (no upward search): a working directory, a linked worktree, a `.git`
/// directory or a bare repository.
pub fn open(path: &Path) -> Result<Repository> {
    Repository::open(path).map_err(|e| match e.code() {
        ErrorCode::NotFound => GitYasError::NotFound(format!("{} is not a git repository", path.display())),
        _ => git_error(e),
    })
}

/// Opens the repository containing `path`, searching parent directories (for "add repository" from a subfolder).
pub fn discover(path: &Path) -> Result<Repository> {
    Repository::discover(path).map_err(|e| match e.code() {
        ErrorCode::NotFound => GitYasError::NotFound(format!("no git repository found at or above {}", path.display())),
        _ => git_error(e),
    })
}

/// Creates a new repository. `initial_branch` defaults to libgit2's / `init.defaultBranch` choice.
pub fn init(path: &Path, bare: bool, initial_branch: Option<&str>) -> Result<Repository> {
    let mut opts = RepositoryInitOptions::new();
    opts.bare(bare).mkpath(true);
    if let Some(branch) = initial_branch {
        if !git2::Branch::name_is_valid(branch).map_err(git_error)? {
            return Err(GitYasError::InvalidInput(format!("invalid branch name: {branch:?}")));
        }
        opts.initial_head(branch);
    }
    Repository::init_opts(path, &opts).map_err(git_error)
}

pub fn state(repo: &Repository) -> RepoState {
    RepoState::from_git(repo.state())
}

impl From<RepoState> for RepoOperation {
    fn from(state: RepoState) -> Self {
        match state {
            RepoState::Clean => RepoOperation::None,
            RepoState::Merge => RepoOperation::Merge,
            RepoState::Revert => RepoOperation::Revert,
            RepoState::CherryPick => RepoOperation::CherryPick,
            RepoState::Bisect => RepoOperation::Bisect,
            RepoState::Rebase => RepoOperation::Rebase,
            RepoState::ApplyMailbox => RepoOperation::ApplyMailbox,
        }
    }
}

/// Operation in progress, shallow / bare flags and the prepared message (`MERGE_MSG`) for the UI.
pub fn status(repo: &Repository) -> Result<RepoStatus> {
    // Read MERGE_MSG ourselves: `git2::Repository::message` unwraps its UTF-8 conversion and would panic on a
    // non-UTF-8 message. A missing file is the normal case; a non-UTF-8 one is simply not offered as a prefill.
    let path = repo.path().join("MERGE_MSG");
    let prepared_message = match std::fs::read(&path) {
        Ok(bytes) => String::from_utf8(bytes).ok(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(GitYasError::Io(format!("cannot read {}: {e}", path.display()))),
    };
    Ok(RepoStatus {
        operation: state(repo).into(),
        is_shallow: repo.is_shallow(),
        is_bare: repo.is_bare(),
        prepared_message,
    })
}

pub fn is_merge_in_progress(repo: &Repository) -> bool {
    state(repo) == RepoState::Merge
}

pub fn is_rebase_in_progress(repo: &Repository) -> bool {
    state(repo) == RepoState::Rebase
}

pub fn info(repo: &Repository) -> Result<RepositoryInfo> {
    let head_unborn = match repo.head() {
        Ok(_) => false,
        Err(e) if matches!(e.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => true,
        Err(e) => return Err(git_error(e)),
    };
    let head_detached = repo.head_detached().map_err(git_error)?;
    Ok(RepositoryInfo {
        git_dir: repo.path().to_path_buf(),
        workdir: repo.workdir().map(Path::to_path_buf),
        is_bare: repo.is_bare(),
        is_shallow: repo.is_shallow(),
        is_worktree: repo.is_worktree(),
        head_unborn,
        head_detached,
        state: state(repo),
    })
}

/// Display name: the working-directory folder name, or the bare repo folder without `.git`.
/// Non-UTF-8 folder names are shown lossily (display only, never used to address the repository).
pub fn repository_name(repo: &Repository) -> Result<String> {
    let dir = match repo.workdir() {
        Some(w) => w,
        None => repo.path(),
    };
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| GitYasError::InvalidInput(format!("cannot derive a name from {}", dir.display())))?;
    let name = name
        .strip_suffix(".git")
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .unwrap_or(name);
    Ok(name)
}

/// Names of all configured remotes, in config order. Remotes with non-UTF-8 names are skipped (libgit2 can't
/// address them by `&str` anyway) and logged.
pub fn remote_names(repo: &Repository) -> Result<Vec<String>> {
    let remotes = repo.remotes().map_err(git_error)?;
    let mut names = Vec::with_capacity(remotes.len());
    for (i, name) in remotes.iter().enumerate() {
        match name {
            Some(n) => names.push(n.to_owned()),
            None => tracing::warn!(index = i, "skipping remote with a non-UTF-8 name"),
        }
    }
    Ok(names)
}

pub fn list_remotes(repo: &Repository) -> Result<Vec<RemoteInfo>> {
    remote_names(repo)?
        .into_iter()
        .map(|name| {
            let remote = repo.find_remote(&name).map_err(git_error)?;
            let url = remote.url().map(str::to_owned);
            let push_url = remote.pushurl().map(str::to_owned);
            let effective = push_url.as_deref().or(url.as_deref());
            Ok(RemoteInfo {
                provider: effective.and_then(detect_provider),
                is_ssh: effective.is_some_and(is_ssh_url),
                name,
                url,
                push_url,
            })
        })
        .collect()
}

/// Where `clone` should put `url` inside `base_dir`: `base_dir/<repo name from URL>`.
///
/// Fixes the old `clone_repository` command, which cloned into this path but then registered `base_dir`.
pub fn clone_destination(url: &str, base_dir: &Path) -> Result<PathBuf> {
    let invalid = || {
        GitYasError::InvalidInput(format!(
            "cannot determine a repository name from {:?}",
            crate::remote::redact_url_credentials(url)
        ))
    };
    let trimmed = url.split(['?', '#']).next().unwrap_or(url).trim();
    // scp-like `git@host:owner/repo.git` has no `/` before the `:`.
    let path_part = match trimmed.split_once("://") {
        Some((_, rest)) => rest,
        None => match trimmed.split_once(':') {
            Some((host, rest)) if !host.contains('/') && host.len() > 1 => rest,
            _ => trimmed,
        },
    };
    let mut segments = path_part.split(['/', '\\']).filter(|s| !s.is_empty()).collect::<Vec<_>>();
    if segments.last() == Some(&".git") {
        segments.pop();
    }
    // A bare `host` (no path) is not a repository name.
    if trimmed.contains("://") && segments.len() < 2 {
        return Err(invalid());
    }
    let last = segments.last().ok_or_else(invalid)?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty() || name == "." || name == ".." || name.contains(':') {
        return Err(invalid());
    }
    Ok(base_dir.join(name))
}

/// Clones `url` into `dest` (which must not exist or be an empty directory). Authenticated like fetch;
/// SSH URLs use ssh-agent or `~/.ssh` keys. Honors `ctx.cancel` (returns `GitYasError::Cancelled`) and reports `ctx.progress`.
pub fn clone(url: &str, dest: &Path, ctx: &RemoteContext<'_>) -> Result<Repository> {
    if let Ok(mut entries) = std::fs::read_dir(dest) {
        if entries.next().is_some() {
            return Err(GitYasError::InvalidInput(format!(
                "{} already exists and is not empty",
                dest.display()
            )));
        }
    } else if dest.exists() {
        return Err(GitYasError::InvalidInput(format!(
            "{} already exists and is not a directory",
            dest.display()
        )));
    }
    let session = Session::new(ctx, url, ConfigSource::Default)?;
    session.check_cancelled()?;
    if crate::remote::use_gitoxide(url, None) {
        let existed = dest.is_dir();
        if let Err(err) = crate::remote::clone_with_gitoxide(url, dest, ctx, &session) {
            // gitoxide removes the destination of a failed clone, even an empty folder that was already there.
            if existed
                && !dest.exists()
                && let Err(io) = std::fs::create_dir(dest)
            {
                tracing::warn!(path = %dest.display(), "cannot restore the empty destination folder: {io}");
            }
            return Err(err);
        }
        let repo = open(dest)?;
        // Windows: let the git CLI handle tracked paths over MAX_PATH (gitoxide's checkout already does).
        if cfg!(windows) {
            repo.config()
                .and_then(|mut config| config.set_bool("core.longpaths", true))
                .map_err(git_error)?;
        }
        return Ok(repo);
    }
    let mut fetch = git2::FetchOptions::new();
    fetch.remote_callbacks(session.callbacks());
    let mut builder = git2::build::RepoBuilder::new();
    builder.fetch_options(fetch);
    // Windows: opt the new repository into `core.longpaths` before checkout, otherwise any tracked path over
    // MAX_PATH (260 chars) fails the whole clone with "path too long" (git-for-windows needs the same flag).
    if cfg!(windows) {
        builder.remote_create(|repo, name, url| {
            repo.config()?.set_bool("core.longpaths", true)?;
            repo.remote(name, url)
        });
    }
    builder.clone(url, dest).map_err(|e| session.map_error(e))
}

// ---- crate-internal helpers shared by the repository modules -------------------------------------------------------

/// The repository index, re-read from disk if another process (CLI, other app) changed it since libgit2 cached it.
pub(crate) fn fresh_index(repo: &Repository) -> Result<git2::Index> {
    let mut index = repo.index().map_err(git_error)?;
    index.read(false).map_err(git_error)?;
    Ok(index)
}

/// Does reference `name` exist? Lookup errors other than "not found" are propagated.
pub(crate) fn ref_exists(repo: &Repository, name: &str) -> Result<bool> {
    match repo.find_reference(name) {
        Ok(_) => Ok(true),
        Err(e) if e.code() == ErrorCode::NotFound => Ok(false),
        Err(e) => Err(git_error(e)),
    }
}

/// HEAD's tree, `None` on an unborn HEAD.
pub(crate) fn head_tree(repo: &Repository) -> Result<Option<git2::Tree<'_>>> {
    match repo.head() {
        Ok(h) => h.peel_to_tree().map(Some).map_err(git_error),
        Err(e) if matches!(e.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => Ok(None),
        Err(e) => Err(git_error(e)),
    }
}

pub(crate) fn ensure_not_bare(repo: &Repository, action: &str) -> Result<()> {
    if repo.is_bare() {
        return Err(GitYasError::InvalidInput(format!("cannot {action} in a bare repository")));
    }
    Ok(())
}

pub(crate) fn ensure_clean_state(repo: &Repository, action: &str) -> Result<()> {
    let st = state(repo);
    if st != RepoState::Clean {
        return Err(GitYasError::InvalidInput(format!(
            "cannot {action} while {} is in progress; finish or abort it first",
            st.describe()
        )));
    }
    Ok(())
}

/// `CommitSignature` -> libgit2 signature, refusing empty names/emails instead of panicking.
pub(crate) fn git_signature(signature: Option<&CommitSignature>, action: &str) -> Result<git2::Signature<'static>> {
    let sig = signature
        .filter(|s| s.is_valid())
        .ok_or_else(|| GitYasError::SignatureRequired(format!("set a commit signature (name and email) before you {action}")))?;
    git2::Signature::now(sig.name.trim(), sig.email.trim())
        .map_err(|e| GitYasError::SignatureRequired(format!("invalid commit signature: {}", e.message())))
}

pub(crate) fn signature_from_git(sig: &git2::Signature<'_>) -> CommitSignature {
    // Display-only identity: non-UTF-8 bytes are replaced (never used to address anything).
    CommitSignature {
        name: String::from_utf8_lossy(sig.name_bytes()).into_owned(),
        email: String::from_utf8_lossy(sig.email_bytes()).into_owned(),
    }
}

/// Index/tree path bytes -> `PathBuf` without lossy conversion on Unix; elsewhere paths must be UTF-8.
pub(crate) fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(bytes)
            .map(PathBuf::from)
            .map_err(|_| GitYasError::InvalidInput(format!("path is not valid UTF-8: {}", String::from_utf8_lossy(bytes))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_destination_forms() -> Result<()> {
        let base = Path::new("/base");
        for (url, name) in [
            ("https://github.com/owner/repo.git", "repo"),
            ("https://github.com/owner/repo", "repo"),
            ("https://github.com/owner/repo/", "repo"),
            ("https://github.com/owner/repo.git?x=1#frag", "repo"),
            ("git@github.com:owner/repo.git", "repo"),
            ("ssh://git@host:22/owner/repo.git", "repo"),
            ("https://dev.azure.com/org/project/_git/my-repo", "my-repo"),
            ("file:///tmp/remote.git", "remote"),
            ("/tmp/some/repo/.git", "repo"),
            ("C:\\code\\proj", "proj"),
        ] {
            assert_eq!(clone_destination(url, base)?, base.join(name), "{url}");
        }
        for bad in ["", "https://github.com", "https://github.com/", "..", "git@host:"] {
            assert!(clone_destination(bad, base).is_err(), "{bad:?}");
        }
        Ok(())
    }

    #[test]
    fn signature_must_be_valid() {
        assert!(git_signature(None, "merge").is_err());
        let empty = CommitSignature {
            name: " ".into(),
            email: "a@b".into(),
        };
        assert!(matches!(
            git_signature(Some(&empty), "merge"),
            Err(GitYasError::SignatureRequired(_))
        ));
        let ok = CommitSignature {
            name: "A".into(),
            email: "a@b".into(),
        };
        assert!(git_signature(Some(&ok), "merge").is_ok());
    }

    #[test]
    fn status_reports_operation_and_prepared_message() -> Result<()> {
        let dir = tempfile::tempdir().map_err(|e| GitYasError::Io(e.to_string()))?;
        let repo = init(dir.path(), false, Some("main"))?;
        let clean = status(&repo)?;
        assert_eq!(clean.operation, RepoOperation::None);
        assert!(!clean.is_shallow && !clean.is_bare && clean.prepared_message.is_none());

        let write = |name: &str, bytes: &[u8]| std::fs::write(repo.path().join(name), bytes).map_err(|e| GitYasError::Io(e.to_string()));
        write("MERGE_MSG", b"Merge branch 'x' into main\n")?;
        assert_eq!(status(&repo)?.prepared_summary(), Some("Merge branch 'x' into main"));
        // Non-UTF-8 MERGE_MSG: no prefill, no panic (git2's `message()` would panic here).
        write("MERGE_MSG", b"Merge caf\xe9\n")?;
        assert_eq!(status(&repo)?.prepared_message, None);

        let bare_dir = tempfile::tempdir().map_err(|e| GitYasError::Io(e.to_string()))?;
        assert!(status(&init(bare_dir.path(), true, None)?)?.is_bare);
        Ok(())
    }
}
