use serde::{Deserialize, Serialize};

use crate::Oid;

/// Where HEAD points. Unborn is an explicit state (fresh repo before its first commit).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Head {
    Branch { name: String, refname: String, target: Oid },
    Detached { target: Oid },
    Unborn { name: String, refname: String },
}

impl Head {
    pub fn target(&self) -> Option<&Oid> {
        match self {
            Head::Branch { target, .. } | Head::Detached { target } => Some(target),
            Head::Unborn { .. } => None,
        }
    }

    pub fn is_detached(&self) -> bool {
        matches!(self, Head::Detached { .. })
    }

    /// Fresh repository (or orphan branch) before its first commit.
    pub fn is_unborn(&self) -> bool {
        matches!(self, Head::Unborn { .. })
    }

    /// Short branch name HEAD is on (also for an unborn branch); `None` when detached.
    pub fn branch_name(&self) -> Option<&str> {
        match self {
            Head::Branch { name, .. } | Head::Unborn { name, .. } => Some(name),
            Head::Detached { .. } => None,
        }
    }

    /// Full ref name HEAD points at (`refs/heads/main`); `None` when detached.
    pub fn refname(&self) -> Option<&str> {
        match self {
            Head::Branch { refname, .. } | Head::Unborn { refname, .. } => Some(refname),
            Head::Detached { .. } => None,
        }
    }

    /// True if HEAD is on the branch with this full ref name (born or unborn).
    pub fn is_on(&self, refname: &str) -> bool {
        self.refname() == Some(refname)
    }

    /// Text for the UI: `main`, `main (no commits yet)` or `HEAD detached at 1a2b3c4`.
    pub fn label(&self) -> String {
        match self {
            Head::Branch { name, .. } => name.clone(),
            Head::Unborn { name, .. } => format!("{name} (no commits yet)"),
            Head::Detached { target } => format!("HEAD detached at {}", target.short(7)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{oid, round_trip};

    fn all() -> [Head; 3] {
        [
            Head::Branch {
                name: "main".into(),
                refname: "refs/heads/main".into(),
                target: oid(1),
            },
            Head::Detached { target: oid(0xab) },
            Head::Unborn {
                name: "main".into(),
                refname: "refs/heads/main".into(),
            },
        ]
    }

    #[test]
    fn accessors() {
        let [branch, detached, unborn] = all();
        assert_eq!(branch.target(), Some(&oid(1)));
        assert_eq!(unborn.target(), None);
        assert!(detached.is_detached() && !branch.is_detached());
        assert!(unborn.is_unborn() && !branch.is_unborn());
        assert_eq!(branch.branch_name(), Some("main"));
        assert_eq!(unborn.branch_name(), Some("main"));
        assert_eq!(detached.branch_name(), None);
        assert!(branch.is_on("refs/heads/main") && unborn.is_on("refs/heads/main"));
        assert!(!detached.is_on("refs/heads/main") && !branch.is_on("refs/heads/dev"));
    }

    #[test]
    fn labels() {
        let [branch, detached, unborn] = all();
        assert_eq!(branch.label(), "main");
        assert_eq!(unborn.label(), "main (no commits yet)");
        assert_eq!(detached.label(), "HEAD detached at abababa");
    }

    #[test]
    fn serde_round_trip_is_internally_tagged() {
        let jsons: Vec<String> = all().iter().map(round_trip).collect();
        assert!(jsons[0].starts_with(r#"{"state":"branch""#), "{}", jsons[0]);
        assert!(jsons[1].starts_with(r#"{"state":"detached""#), "{}", jsons[1]);
        assert!(jsons[2].starts_with(r#"{"state":"unborn""#), "{}", jsons[2]);
    }
}
