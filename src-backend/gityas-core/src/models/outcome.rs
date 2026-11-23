//! Results of git operations, returned by the engine so the UI can show "up to date", conflicts, what moved, etc.
//! Produced by `gityas_git` (`remote`, `stash`). Transient values: not persisted, so no serde.

use std::path::PathBuf;

use crate::{GitYasError, Oid};

/// One ref changed by a fetch. `old: None` = created, `new: None` = deleted (pruned).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    pub refname: String,
    pub old: Option<Oid>,
    pub new: Option<Oid>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFetch {
    pub remote: String,
    pub updated: Vec<RefUpdate>,
}

/// Result of fetching one or all remotes. A repository without remotes yields an empty summary (the
/// old code panicked on `remotes.get(0).unwrap()`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchSummary {
    pub fetched: Vec<RemoteFetch>,
    /// Remotes that failed while others succeeded (all failing is an `Err`).
    pub failed: Vec<(String, GitYasError)>,
}

impl FetchSummary {
    pub fn updated_any(&self) -> bool {
        self.fetched.iter().any(|f| !f.updated.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushOutcome {
    pub remote: String,
    /// Full local ref pushed, e.g. `refs/heads/main`.
    pub local_ref: String,
    /// Full ref updated on the remote, e.g. `refs/heads/main`.
    pub remote_ref: String,
    /// True if the branch had no upstream and it was set by this push.
    pub upstream_set: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    UpToDate,
    /// `from` is `None` when the target branch was unborn.
    FastForward {
        from: Option<Oid>,
        to: Oid,
    },
    /// A merge commit was created.
    Merged {
        commit: Oid,
    },
    /// Conflicts were written to the index and working tree; the repository is in the merge state.
    Conflicts {
        paths: Vec<PathBuf>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbortMergeOutcome {
    /// Paths restored to their HEAD version.
    pub restored: Vec<PathBuf>,
    /// Paths the merge introduced (absent from HEAD) that were removed from the index and working tree.
    pub removed: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullOutcome {
    pub fetch: RemoteFetch,
    pub merge: MergeOutcome,
}

/// Result of applying (or popping) a stash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StashApplyOutcome {
    Applied,
    /// The stash was applied with conflicts (markers in the files, conflicts in the index). A pop does **not** drop
    /// the stash in this case (libgit2's own `stash_pop` would).
    Conflicts {
        paths: Vec<PathBuf>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::oid;

    #[test]
    fn fetch_summary_updated_any() {
        let mut summary = FetchSummary::default();
        assert!(!summary.updated_any());
        summary.fetched.push(RemoteFetch {
            remote: "origin".into(),
            updated: Vec::new(),
        });
        assert!(!summary.updated_any());
        summary.fetched.push(RemoteFetch {
            remote: "upstream".into(),
            updated: vec![RefUpdate {
                refname: "refs/remotes/upstream/main".into(),
                old: None,
                new: Some(oid(1)),
            }],
        });
        assert!(summary.updated_any());
    }
}
