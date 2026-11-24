use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Oid;

/// A working tree of the repository (`git worktree list`): the main one, or a linked one created with
/// `git worktree add`. Not to be confused with the working-tree *files* (`FileState`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// Name of a linked worktree (its admin dir `.git/worktrees/<name>`); `None` for the main worktree.
    pub name: Option<String>,
    /// Folder of the working tree.
    pub path: PathBuf,
    /// Short name of the checked-out branch; `None` when detached (or unreadable).
    pub branch: Option<String>,
    /// Commit HEAD points at; `None` for an unborn branch or an unreadable worktree.
    pub head: Option<Oid>,
    /// The repository open in this session is this worktree.
    pub is_current: bool,
    /// Locked (`git worktree lock`): git won't prune or remove it without force. The reason, if one was given, is
    /// in `lock_reason`.
    pub locked: bool,
    pub lock_reason: Option<String>,
    /// Its folder is gone (moved or deleted outside git): only the admin data is left, and removing it prunes that.
    pub prunable: bool,
}

impl Worktree {
    pub fn is_main(&self) -> bool {
        self.name.is_none()
    }

    /// Last component of the folder ("GitYas-feature"), the label shown in lists.
    pub fn folder_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

/// What a new worktree checks out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum WorktreeBranch {
    /// An existing local branch (full ref name). Git refuses a branch checked out in another worktree.
    Existing { refname: String },
    /// A new local branch `name`, starting at `from` (`None` = HEAD).
    New { name: String, from: Option<Oid> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_name_is_the_last_component() {
        let wt = Worktree {
            name: Some("feature".into()),
            path: PathBuf::from("/code/app-feature"),
            branch: Some("feature".into()),
            head: None,
            is_current: false,
            locked: false,
            lock_reason: None,
            prunable: false,
        };
        assert_eq!(wt.folder_name(), "app-feature");
        assert!(!wt.is_main());
    }
}
