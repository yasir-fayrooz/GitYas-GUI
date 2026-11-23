use serde::{Deserialize, Serialize};

use crate::Oid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchKind {
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    /// Short name as shown, e.g. `main` or `origin/main`.
    pub name: String,
    /// Full ref name, e.g. `refs/heads/main`; commands take this to avoid ambiguity.
    pub refname: String,
    pub kind: BranchKind,
    /// `None` for symbolic refs without a resolvable target (e.g. a dangling `origin/HEAD`).
    pub target: Option<Oid>,
    /// Remote name for remote branches, or the upstream's remote for local branches.
    pub remote_name: Option<String>,
    /// Short name of the upstream (`origin/main`) for local branches.
    pub upstream: Option<String>,
    /// Full ref name of the upstream (`refs/remotes/origin/main`) for local branches.
    pub upstream_refname: Option<String>,
    /// An upstream is configured but its remote-tracking ref no longer exists (deleted on the remote and pruned).
    /// `upstream`/`upstream_refname` still name it; `ahead`/`behind` are 0.
    pub upstream_gone: bool,
    pub ahead: usize,
    pub behind: usize,
    /// True for the branch HEAD points at.
    pub is_head: bool,
}

impl BranchKind {
    /// Full-name prefix of this kind of branch.
    pub fn refname_prefix(self) -> &'static str {
        match self {
            BranchKind::Local => "refs/heads/",
            BranchKind::Remote => "refs/remotes/",
        }
    }

    /// `Some` for `refs/heads/*` and `refs/remotes/*`, `None` for anything else.
    pub fn from_refname(refname: &str) -> Option<Self> {
        [BranchKind::Local, BranchKind::Remote]
            .into_iter()
            .find(|kind| refname.strip_prefix(kind.refname_prefix()).is_some_and(|rest| !rest.is_empty()))
    }
}

impl Branch {
    pub fn is_local(&self) -> bool {
        self.kind == BranchKind::Local
    }

    pub fn is_remote(&self) -> bool {
        self.kind == BranchKind::Remote
    }

    pub fn has_upstream(&self) -> bool {
        self.upstream.is_some()
    }

    /// Both ahead of and behind the upstream (a plain fast-forward is impossible).
    pub fn is_diverged(&self) -> bool {
        self.ahead > 0 && self.behind > 0
    }

    /// Has a live upstream and is neither ahead nor behind it.
    pub fn is_up_to_date(&self) -> bool {
        self.has_upstream() && !self.upstream_gone && self.ahead == 0 && self.behind == 0
    }

    /// `refs/remotes/<remote>/HEAD`: a symbolic pointer to the remote's default branch. Should not be listed as a
    /// branch (the old code only skipped the literal `origin/HEAD`).
    pub fn is_remote_head(&self) -> bool {
        self.is_remote() && self.refname.ends_with("/HEAD")
    }

    /// The branch name on the remote: `origin/feature/x` → `feature/x`. For local branches (or a remote branch
    /// whose name doesn't start with `<remote_name>/`) this is `name` unchanged. Never panics; the old
    /// `name_without_remote` dropped the first character when `remote_name` was `None` and could panic on short names.
    pub fn name_without_remote(&self) -> &str {
        match (&self.kind, &self.remote_name) {
            (BranchKind::Remote, Some(remote)) => self
                .name
                .strip_prefix(remote.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
                .filter(|rest| !rest.is_empty())
                .unwrap_or(&self.name),
            _ => &self.name,
        }
    }

    /// Push refspec that deletes this remote branch on its remote (`:refs/heads/<name>`); `None` for local branches
    /// or when the remote is unknown.
    pub fn remote_delete_refspec(&self) -> Option<String> {
        (self.is_remote() && self.remote_name.is_some()).then(|| format!(":refs/heads/{}", self.name_without_remote()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{oid, round_trip};

    fn local(name: &str) -> Branch {
        Branch {
            name: name.into(),
            refname: format!("refs/heads/{name}"),
            kind: BranchKind::Local,
            target: Some(oid(1)),
            remote_name: Some("origin".into()),
            upstream: Some(format!("origin/{name}")),
            upstream_refname: Some(format!("refs/remotes/origin/{name}")),
            upstream_gone: false,
            ahead: 0,
            behind: 0,
            is_head: true,
        }
    }

    fn remote(name: &str, remote_name: Option<&str>) -> Branch {
        Branch {
            name: name.into(),
            refname: format!("refs/remotes/{name}"),
            kind: BranchKind::Remote,
            target: None,
            remote_name: remote_name.map(Into::into),
            upstream: None,
            upstream_refname: None,
            upstream_gone: false,
            ahead: 0,
            behind: 0,
            is_head: false,
        }
    }

    #[test]
    fn kind_prefix_round_trip() {
        assert_eq!(BranchKind::from_refname("refs/heads/main"), Some(BranchKind::Local));
        assert_eq!(BranchKind::from_refname("refs/remotes/origin/main"), Some(BranchKind::Remote));
        assert_eq!(BranchKind::from_refname("refs/tags/v1"), None);
        assert_eq!(BranchKind::from_refname("refs/heads/"), None);
        for kind in [BranchKind::Local, BranchKind::Remote] {
            assert_eq!(BranchKind::from_refname(&format!("{}x", kind.refname_prefix())), Some(kind));
        }
    }

    #[test]
    fn name_without_remote_never_mangles() {
        assert_eq!(remote("origin/feature/x", Some("origin")).name_without_remote(), "feature/x");
        assert_eq!(remote("my/remote/main", Some("my/remote")).name_without_remote(), "main");
        // Old code: drained `0 + 1` chars, giving "rigin/main".
        assert_eq!(remote("origin/main", None).name_without_remote(), "origin/main");
        // Old code: panicked (drain past the end).
        assert_eq!(remote("o", Some("origin")).name_without_remote(), "o");
        assert_eq!(remote("origin/", Some("origin")).name_without_remote(), "origin/");
        assert_eq!(local("main").name_without_remote(), "main");
    }

    #[test]
    fn delete_refspec() {
        assert_eq!(
            remote("origin/feature/x", Some("origin")).remote_delete_refspec().as_deref(),
            Some(":refs/heads/feature/x")
        );
        assert_eq!(remote("origin/x", None).remote_delete_refspec(), None);
        assert_eq!(local("main").remote_delete_refspec(), None);
    }

    #[test]
    fn tracking_state() {
        let mut b = local("main");
        assert!(b.is_local() && b.has_upstream() && b.is_up_to_date() && !b.is_diverged());
        b.ahead = 2;
        assert!(!b.is_up_to_date() && !b.is_diverged());
        b.behind = 1;
        assert!(b.is_diverged());
        b.upstream = None;
        b.ahead = 0;
        b.behind = 0;
        assert!(!b.is_up_to_date());
    }

    #[test]
    fn remote_head_detection() {
        assert!(remote("upstream/HEAD", Some("upstream")).is_remote_head());
        assert!(!remote("origin/main", Some("origin")).is_remote_head());
        assert!(!local("HEAD").is_remote_head());
    }

    #[test]
    fn serde_round_trip() {
        round_trip(&local("main"));
        let json = round_trip(&remote("origin/main", Some("origin")));
        assert!(json.contains(r#""kind":"remote""#), "{json}");
        assert!(json.contains(r#""target":null"#), "{json}");
    }

    #[test]
    fn gone_upstream_is_not_up_to_date() {
        let mut b = local("main");
        assert!(b.is_up_to_date());
        b.upstream_gone = true;
        assert!(b.has_upstream() && !b.is_up_to_date());
    }
}
