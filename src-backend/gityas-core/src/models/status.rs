//! Repository-level state the UI shows outside the file lists (Wave 3): an operation in progress (merge banner,
//! commit box prefill) and history caveats (shallow clone notice).

use serde::{Deserialize, Serialize};

/// Operation recorded in the git dir (`MERGE_HEAD`, `rebase-merge/`, ...).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoOperation {
    #[default]
    None,
    Merge,
    Revert,
    CherryPick,
    Bisect,
    Rebase,
    ApplyMailbox,
}

/// Pushed as [`AppEvent::RepoStatusUpdated`](crate::AppEvent::RepoStatusUpdated).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoStatus {
    pub operation: RepoOperation,
    pub is_shallow: bool,
    pub is_bare: bool,
    /// Prepared commit message (`MERGE_MSG`) while a merge / revert / cherry-pick is in progress, e.g.
    /// `Merge branch 'x' into main`; prefills the commit box. `None` if there is none or it is not valid UTF-8.
    pub prepared_message: Option<String>,
}

impl RepoOperation {
    /// Short description for a banner: "a merge", "a rebase", ...; `None` when nothing is in progress.
    pub fn describe(self) -> Option<&'static str> {
        match self {
            RepoOperation::None => None,
            RepoOperation::Merge => Some("a merge"),
            RepoOperation::Revert => Some("a revert"),
            RepoOperation::CherryPick => Some("a cherry-pick"),
            RepoOperation::Bisect => Some("a bisect"),
            RepoOperation::Rebase => Some("a rebase"),
            RepoOperation::ApplyMailbox => Some("a `git am` session"),
        }
    }
}

impl RepoStatus {
    pub fn is_merging(&self) -> bool {
        self.operation == RepoOperation::Merge
    }

    /// First line of the prepared message (the commit summary prefill).
    pub fn prepared_summary(&self) -> Option<&str> {
        self.prepared_message
            .as_deref()
            .and_then(|m| m.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::round_trip;

    #[test]
    fn describe_and_prefill() {
        assert_eq!(RepoOperation::None.describe(), None);
        assert_eq!(RepoOperation::Merge.describe(), Some("a merge"));
        let status = RepoStatus {
            operation: RepoOperation::Merge,
            prepared_message: Some("\n# comment\nMerge branch 'x' into main\n\n# Conflicts:\n#\tsrc/lib.rs\n".into()),
            ..RepoStatus::default()
        };
        assert!(status.is_merging());
        assert_eq!(status.prepared_summary(), Some("Merge branch 'x' into main"));
        assert_eq!(RepoStatus::default().prepared_summary(), None);
        round_trip(&status);
        round_trip(&RepoStatus::default());
    }
}
