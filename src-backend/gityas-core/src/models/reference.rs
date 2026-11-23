use serde::{Deserialize, Serialize};

use crate::Oid;
use crate::models::BranchKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    Branch(BranchKind),
    Tag,
    Note,
    Stash,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    /// Full name, e.g. `refs/tags/v1.0`.
    pub name: String,
    pub short_name: String,
    pub kind: ReferenceKind,
    /// Peeled to a commit (annotated tags resolve to their commit).
    pub target: Oid,
}

/// Full-name prefixes, in the order libgit2's `git_reference_shorthand` strips them.
const SHORTHAND_PREFIXES: [&str; 4] = ["refs/heads/", "refs/tags/", "refs/remotes/", "refs/"];

/// The short form of a full ref name, matching libgit2's `git_reference_shorthand`
/// (`refs/heads/main` → `main`, `refs/remotes/origin/main` → `origin/main`, `refs/notes/commits` → `notes/commits`,
/// `refs/stash` → `stash`). Names outside `refs/` (e.g. `HEAD`) are returned unchanged.
pub fn shorten_refname(refname: &str) -> &str {
    SHORTHAND_PREFIXES
        .iter()
        .find_map(|prefix| refname.strip_prefix(prefix).filter(|rest| !rest.is_empty()))
        .unwrap_or(refname)
}

/// Splits a remote-tracking short name (`origin/feature/x`) into `(remote, branch)` using the configured remote
/// names. Remote names may themselves contain `/`, so the longest matching remote wins; `None` if no remote matches
/// or the branch part would be empty.
pub fn split_remote_branch<'a, S: AsRef<str>>(short_name: &'a str, remotes: &[S]) -> Option<(&'a str, &'a str)> {
    remotes
        .iter()
        .map(AsRef::as_ref)
        .filter(|remote| !remote.is_empty())
        .filter_map(|remote| {
            let branch = short_name.strip_prefix(remote)?.strip_prefix('/')?;
            let remote = short_name.get(..remote.len())?;
            (!branch.is_empty()).then_some((remote, branch))
        })
        .max_by_key(|(remote, _)| remote.len())
}

impl ReferenceKind {
    /// Classifies a full ref name. Unlike the old `GitYasReferenceType`, remote-tracking branches are branches
    /// (the old code called `is_branch()`, which is `refs/heads/` only, so `refs/remotes/*` became `NOTE`).
    pub fn from_refname(refname: &str) -> Self {
        if refname.starts_with("refs/heads/") {
            ReferenceKind::Branch(BranchKind::Local)
        } else if refname.starts_with("refs/remotes/") {
            ReferenceKind::Branch(BranchKind::Remote)
        } else if refname.starts_with("refs/tags/") {
            ReferenceKind::Tag
        } else if refname.starts_with("refs/notes/") {
            ReferenceKind::Note
        } else if refname == "refs/stash" {
            ReferenceKind::Stash
        } else {
            ReferenceKind::Other
        }
    }

    pub fn is_branch(self) -> bool {
        matches!(self, ReferenceKind::Branch(_))
    }

    pub fn is_tag(self) -> bool {
        self == ReferenceKind::Tag
    }
}

impl Reference {
    /// Builds a reference from its full name, deriving `short_name` and `kind`. `target` must already be peeled.
    pub fn from_refname(name: impl Into<String>, target: Oid) -> Self {
        let name = name.into();
        Self {
            short_name: shorten_refname(&name).to_owned(),
            kind: ReferenceKind::from_refname(&name),
            name,
            target,
        }
    }

    pub fn is_branch(&self) -> bool {
        self.kind.is_branch()
    }

    pub fn is_tag(&self) -> bool {
        self.kind.is_tag()
    }

    /// `refs/remotes/<remote>/HEAD`: a symbolic pointer to the remote's default branch, not a branch to list.
    pub fn is_remote_head(&self) -> bool {
        self.kind == ReferenceKind::Branch(BranchKind::Remote) && self.name.ends_with("/HEAD")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{oid, round_trip};

    #[test]
    fn shorten_matches_libgit2() {
        assert_eq!(shorten_refname("refs/heads/main"), "main");
        assert_eq!(shorten_refname("refs/heads/feature/x"), "feature/x");
        assert_eq!(shorten_refname("refs/tags/v1.0"), "v1.0");
        assert_eq!(shorten_refname("refs/remotes/origin/main"), "origin/main");
        assert_eq!(shorten_refname("refs/notes/commits"), "notes/commits");
        assert_eq!(shorten_refname("refs/stash"), "stash");
        assert_eq!(shorten_refname("HEAD"), "HEAD");
        assert_eq!(shorten_refname("refs/heads/"), "heads/");
        assert_eq!(shorten_refname("refs/"), "refs/");
        assert_eq!(shorten_refname(""), "");
    }

    #[test]
    fn classify_refnames() {
        use ReferenceKind as K;
        assert_eq!(K::from_refname("refs/heads/main"), K::Branch(BranchKind::Local));
        assert_eq!(K::from_refname("refs/remotes/origin/main"), K::Branch(BranchKind::Remote));
        assert_eq!(K::from_refname("refs/tags/v1"), K::Tag);
        assert_eq!(K::from_refname("refs/notes/commits"), K::Note);
        assert_eq!(K::from_refname("refs/stash"), K::Stash);
        assert_eq!(K::from_refname("refs/pull/1/head"), K::Other);
        assert_eq!(K::from_refname("HEAD"), K::Other);
        assert!(K::Branch(BranchKind::Remote).is_branch());
        assert!(K::Tag.is_tag());
        assert!(!K::Note.is_branch());
    }

    #[test]
    fn split_remote_prefers_longest_remote() {
        let remotes = ["origin", "origin/mirror", "up"];
        assert_eq!(split_remote_branch("origin/main", &remotes), Some(("origin", "main")));
        assert_eq!(split_remote_branch("origin/mirror/main", &remotes), Some(("origin/mirror", "main")));
        assert_eq!(split_remote_branch("origin/feature/x", &remotes), Some(("origin", "feature/x")));
        assert_eq!(split_remote_branch("upstream/main", &remotes), None);
        assert_eq!(split_remote_branch("origin/", &remotes), None);
        assert_eq!(split_remote_branch("origin", &remotes), None);
        assert_eq!(split_remote_branch("main", &[String::new()]), None);
        let none: [&str; 0] = [];
        assert_eq!(split_remote_branch("origin/main", &none), None);
    }

    #[test]
    fn reference_from_refname() {
        let r = Reference::from_refname("refs/remotes/origin/HEAD", oid(1));
        assert_eq!(r.short_name, "origin/HEAD");
        assert!(r.is_branch());
        assert!(r.is_remote_head());
        let t = Reference::from_refname("refs/tags/v2", oid(2));
        assert!(t.is_tag());
        assert!(!t.is_remote_head());
        assert!(!Reference::from_refname("refs/heads/HEAD", oid(3)).is_remote_head());
    }

    #[test]
    fn serde_round_trip() {
        for name in [
            "refs/heads/a",
            "refs/remotes/o/a",
            "refs/tags/t",
            "refs/notes/n",
            "refs/stash",
            "refs/x",
        ] {
            round_trip(&Reference::from_refname(name, oid(9)));
        }
        let json = round_trip(&Reference::from_refname("refs/remotes/o/a", oid(9)));
        assert!(json.contains(r#""kind":{"branch":"remote"}"#), "{json}");
    }
}
