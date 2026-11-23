//! Row types for the commit-graph cache (§3.4), shared by `gityas-git` (producer) and `gityas-storage` (store).

use serde::{Deserialize, Serialize};

use crate::Oid;
use crate::models::Commit;

/// A cached commit row (`commits` + `commit_parents`). Identical to the listed [`Commit`]: commits are
/// immutable, so the cache stores exactly what is shown.
pub type CommitRow = Commit;

/// One live ref tip, peeled to a commit.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RefTip {
    pub refname: String,
    pub oid: Oid,
}

/// Hash of the sorted `(refname, oid)` set; equal fingerprints mean the cache can be served without walking.
/// The hash function is chosen by WP5 and must be stable across runs (not `DefaultHasher`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RefsFingerprint(pub Vec<u8>);

impl RefTip {
    pub fn new(refname: impl Into<String>, oid: Oid) -> Self {
        Self {
            refname: refname.into(),
            oid,
        }
    }
}

impl RefsFingerprint {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{oid, round_trip};

    #[test]
    fn ref_tips_sort_by_name_then_oid() {
        let mut tips = [RefTip::new("refs/tags/v1", oid(1)), RefTip::new("refs/heads/main", oid(2))];
        tips.sort();
        assert_eq!(tips[0].refname, "refs/heads/main");
    }

    #[test]
    fn serde_round_trip() {
        round_trip(&RefTip::new("refs/heads/main", oid(3)));
        let fp = RefsFingerprint(vec![0, 1, 255]);
        assert_eq!(fp.as_bytes(), [0, 1, 255]);
        round_trip(&fp);
        round_trip(&RefsFingerprint(vec![]));
    }
}
