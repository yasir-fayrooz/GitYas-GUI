use serde::{Deserialize, Serialize};

use crate::Oid;
use crate::models::{ChangeCounts, CommitSignature, Delta};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stash {
    /// Position in the stash list (`stash@{index}`); changes as stashes are added/removed.
    pub index: usize,
    pub id: Oid,
    pub message: String,
    pub author: CommitSignature,
    pub time: i64,
    /// Files added / modified / deleted by the stash (derived from its file list, see [`StashDetails::from_files`]).
    pub counts: ChangeCounts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashDetails {
    pub stash: Stash,
    pub files: Vec<Delta>,
}

impl Stash {
    /// `stash@{index}`, the revspec git uses for this entry.
    pub fn revspec(&self) -> String {
        format!("stash@{{{}}}", self.index)
    }

    pub fn counts(&self) -> ChangeCounts {
        self.counts
    }
}

impl StashDetails {
    /// Builds details with the counts derived from `files`, so the badge numbers can't disagree with the list.
    pub fn from_files(index: usize, id: Oid, message: String, author: CommitSignature, time: i64, files: Vec<Delta>) -> Self {
        let counts = ChangeCounts::from_deltas(&files);
        Self {
            stash: Stash {
                index,
                id,
                message,
                author,
                time,
                counts,
            },
            files,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::DeltaChange;
    use crate::test_util::{oid, round_trip};

    fn details() -> StashDetails {
        StashDetails::from_files(
            2,
            oid(7),
            "WIP on main: 1a2b3c4 msg".into(),
            CommitSignature {
                name: "Ada".into(),
                email: "ada@example.com".into(),
            },
            1_700_000_000,
            vec![
                Delta::new("new.txt", DeltaChange::Untracked),
                Delta::new("mod.txt", DeltaChange::Modified),
                Delta::moved("a", "b", DeltaChange::Renamed),
                Delta::new("gone.txt", DeltaChange::Deleted),
            ],
        )
    }

    #[test]
    fn counts_follow_files() {
        let d = details();
        assert_eq!((d.stash.counts.added, d.stash.counts.modified, d.stash.counts.deleted), (1, 2, 1));
        assert_eq!(d.stash.counts.total(), d.files.len());
        assert_eq!(d.stash.revspec(), "stash@{2}");
    }

    #[test]
    fn serde_round_trip() {
        let d = details();
        round_trip(&d.stash);
        round_trip(&d);
    }
}
