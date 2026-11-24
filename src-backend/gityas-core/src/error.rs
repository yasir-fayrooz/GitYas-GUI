use std::path::PathBuf;

use thiserror::Error;

pub type Result<T, E = GitYasError> = std::result::Result<T, E>;

/// The single error type crossing crate boundaries. `Clone` so it can be stored in UI state and events;
/// variants carry a user-facing message (never a token or other secret).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GitYasError {
    #[error("Git error: {0}")]
    Git(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("Authentication failed: {0}")]
    Auth(String),
    #[error("Storage error: {0}")]
    Storage(String),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("No repository is open")]
    NoActiveRepository,
    /// A hunk's `content_hash` no longer matches the freshly computed diff; the UI should re-fetch it.
    #[error("The file changed since it was displayed; refresh and try again")]
    StaleHunk,
    #[error("Invalid input: {0}")]
    InvalidInput(String),
    /// The thing being created is already there (a registered repository path, a branch or tag name).
    #[error("Already exists: {0}")]
    AlreadyExists(String),
    /// Local changes in these paths would be overwritten (checkout, merge, stash apply); nothing was changed.
    /// Repository-relative paths, for the UI to list.
    #[error("Your local changes to {} would be overwritten; commit or stash them first", describe_paths(.0))]
    Conflict(Vec<PathBuf>),
    /// Deleting a local branch (short name) whose commits are not merged; deleting with force discards them.
    #[error("Branch {0:?} is not fully merged; delete it with force to discard its commits")]
    NotMerged(String),
    /// Removing a worktree (folder name) with uncommitted or untracked changes; removing with force discards them.
    #[error("Worktree {0:?} has uncommitted changes; remove it with force to discard them")]
    WorktreeDirty(String),
    /// The remote refused a non-fast-forward update: pull first, or force push. Other rejections (hooks,
    /// protected branches) are `Git`.
    #[error("Push rejected: {0}")]
    PushRejected(String),
    /// The operation writes a commit or an annotated tag and no valid commit signature is set.
    #[error("{0}")]
    SignatureRequired(String),
    #[error("Operation cancelled")]
    Cancelled,
    #[error("{0}")]
    Other(String),
}

fn describe_paths(paths: &[PathBuf]) -> String {
    const SHOWN: usize = 3;
    let mut text = paths
        .iter()
        .take(SHOWN)
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    match paths.len() {
        0 => text = "some files".into(),
        n if n > SHOWN => text.push_str(&format!(" and {} more", n - SHOWN)),
        _ => {}
    }
    text
}

impl From<std::io::Error> for GitYasError {
    fn from(err: std::io::Error) -> Self {
        GitYasError::Io(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_message_lists_a_few_paths() {
        let paths = |n: usize| (0..n).map(|i| PathBuf::from(format!("f{i}"))).collect::<Vec<_>>();
        assert_eq!(
            GitYasError::Conflict(paths(2)).to_string(),
            "Your local changes to f0, f1 would be overwritten; commit or stash them first"
        );
        assert!(GitYasError::Conflict(paths(5)).to_string().contains("f0, f1, f2 and 2 more"));
        assert!(GitYasError::Conflict(Vec::new()).to_string().contains("some files"));
    }
}
