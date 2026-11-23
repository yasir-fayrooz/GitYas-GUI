//! Resolving a user-chosen folder to the repository record to register (blocking; filesystem + git2).

use std::path::{Path, PathBuf};

use gityas_core::models::Repository;
use gityas_core::{GitYasError, Provider, Result};

/// What `add_repository` / `clone_repository` insert into the user DB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Resolved {
    /// Canonical working-directory root, or the git dir for a bare repository.
    pub(super) path: PathBuf,
    pub(super) name: String,
    pub(super) provider: Option<Provider>,
}

/// `path` may be the repository root, any folder inside its working tree, its `.git` directory, a linked worktree
/// or a bare repository. The registered path is the canonical root (symlinks and `..` resolved, no trailing
/// separator) so the same repository can't be registered twice under different spellings.
pub(super) fn resolve(path: &Path, provider: Option<Provider>) -> Result<Resolved> {
    if path.as_os_str().is_empty() {
        return Err(GitYasError::InvalidInput("Choose a folder".into()));
    }
    require_utf8(path)?;
    let canonical = canonicalize(path)?;
    let repo = gityas_git::repository::discover(&canonical).map_err(|err| match err {
        GitYasError::NotFound(_) => GitYasError::NotFound(format!("{} is not a git repository", path.display())),
        other => other,
    })?;
    let root = match repo.workdir() {
        Some(workdir) => workdir,
        None => repo.path(),
    };
    let root = canonicalize(root)?;
    // A UTF-8 subfolder of a non-UTF-8 root (or the reverse): storage would reject it anyway.
    require_utf8(&root)?;
    let name = match Repository::default_name(&root) {
        Some(name) => name,
        None => gityas_git::repository::repository_name(&repo)?,
    };
    let provider = provider.or_else(|| detect_provider(&repo));
    Ok(Resolved {
        path: root,
        name,
        provider,
    })
}

/// Provider of `origin`, else of the first remote whose URL names a known host. Remote listing errors are logged and
/// treated as "unknown": a broken remote config must not prevent registering the repository.
pub(super) fn detect_provider(repo: &git2::Repository) -> Option<Provider> {
    let remotes = match gityas_git::repository::list_remotes(repo) {
        Ok(remotes) => remotes,
        Err(err) => {
            tracing::warn!("cannot list remotes for provider detection: {err}");
            return None;
        }
    };
    remotes
        .iter()
        .find(|r| r.name == "origin")
        .and_then(|r| r.provider)
        .or_else(|| remotes.iter().find_map(|r| r.provider))
}

pub(super) fn require_utf8(path: &Path) -> Result<()> {
    if path.to_str().is_none() {
        return Err(GitYasError::InvalidInput(format!(
            "GitYas does not support folders whose path is not valid UTF-8: {}",
            path.display()
        )));
    }
    Ok(())
}

pub(super) fn canonicalize(path: &Path) -> Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(canonical) => Ok(strip_verbatim(canonical)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(GitYasError::NotFound(format!("{} does not exist", path.display()))),
        Err(err) => Err(GitYasError::Io(format!("cannot resolve {}: {err}", path.display()))),
    }
}

/// Windows `canonicalize` returns `\\?\C:\...`; store the familiar `C:\...` form (UNC `\\?\UNC\...` is kept).
#[cfg(windows)]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}

#[cfg(not(windows))]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    path
}
