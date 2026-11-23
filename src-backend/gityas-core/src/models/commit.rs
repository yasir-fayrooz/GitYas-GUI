use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::Oid;
use crate::models::{Head, Reference, Tag};

/// One commit as listed in the history/graph. Bodies are not included; fetch on demand (`Engine::commit_body`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    pub id: Oid,
    /// In parent order; the first parent is the mainline.
    pub parents: Vec<Oid>,
    pub author_name: String,
    pub author_email: String,
    /// Seconds since the Unix epoch.
    pub author_time: i64,
    pub committer_time: i64,
    /// First line of the message. Non-UTF-8 messages are decoded lossily here; the body API reports it.
    pub summary: String,
    /// Preview of the message body after the summary, whitespace collapsed to one line and capped at
    /// [`Commit::DESCRIPTION_PREVIEW_CHARS`] for the graph row. Empty when there is no body.
    #[serde(default)]
    pub description: String,
    /// `author_name`, `author_email` or `summary` were not valid UTF-8 and contain replacement characters.
    #[serde(default)]
    pub lossy: bool,
}

/// A commit's full message (`Engine::commit_body`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitBody {
    /// The raw message, decoded as UTF-8 (lossily if `lossy`).
    pub message: String,
    /// The message bytes were not valid UTF-8 (e.g. a legacy encoding); `message` contains replacement characters.
    pub lossy: bool,
    /// The commit's declared `encoding` header, if any (e.g. `ISO-8859-1`). Not transcoded.
    pub encoding: Option<String>,
}

/// How `reset_to_commit` treats the index and working tree (the branch moves either way).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResetMode {
    /// `git reset --soft`: only the branch moves; every change since the target is kept (staged).
    Soft,
    /// `git reset --hard`: index and working tree are reset to the target; uncommitted changes are discarded.
    Hard,
}

/// The repository history pushed to the UI: commits in display order plus the live refs to decorate them.
/// Graph layout (lanes, colours) is computed by the frontend (`src-frontend/src/graph.rs`).
/// `Arc` slices so snapshots are shared rather than cloned on every update.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitWrapper {
    pub commits: Arc<[Commit]>,
    /// All refs (branches, tags, notes), always read live, never from the cache.
    pub references: Arc<[Reference]>,
    pub tags: Arc<[Tag]>,
    pub head: Option<Head>,
    /// Only the newest commits: a first page sent while the full history loads (another `CommitsUpdated` with
    /// the full history follows). Commits missing from it may still exist.
    #[serde(default)]
    pub partial: bool,
}

impl Commit {
    /// Maximum length (in chars) of [`Commit::description`].
    pub const DESCRIPTION_PREVIEW_CHARS: usize = 200;

    /// Collapses a message body to the single-line [`Commit::description`] preview.
    pub fn description_preview(body: &str) -> String {
        let mut preview = String::new();
        for word in body.split_whitespace() {
            if !preview.is_empty() {
                preview.push(' ');
            }
            preview.push_str(word);
            if preview.len() >= Self::DESCRIPTION_PREVIEW_CHARS * 4 {
                break;
            }
        }
        match preview.char_indices().nth(Self::DESCRIPTION_PREVIEW_CHARS) {
            Some((end, _)) => preview[..end].to_owned(),
            None => preview,
        }
    }

    /// More than one parent.
    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }

    /// No parents (the first commit, or an orphan branch's first commit).
    pub fn is_root(&self) -> bool {
        self.parents.is_empty()
    }

    /// The mainline parent; diffs of merge commits are against this one.
    pub fn first_parent(&self) -> Option<&Oid> {
        self.parents.first()
    }

    /// Abbreviated id for display (7 hex characters).
    pub fn short_id(&self) -> String {
        self.id.short(7)
    }
}

impl CommitWrapper {
    pub fn len(&self) -> usize {
        self.commits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commits.is_empty()
    }

    /// Row index of each commit, for O(1) lookups (graph edges, "jump to commit"). Build once per snapshot.
    pub fn index_by_id(&self) -> HashMap<&Oid, usize> {
        self.commits.iter().enumerate().map(|(i, c)| (&c.id, i)).collect()
    }

    /// References grouped by the commit they point at, in `references` order, for row decorations
    /// (replaces the old `sha_to_references`, which keyed annotated tags by the tag object instead of the commit).
    pub fn references_by_target(&self) -> HashMap<&Oid, Vec<&Reference>> {
        let mut map: HashMap<&Oid, Vec<&Reference>> = HashMap::new();
        for reference in self.references.iter() {
            map.entry(&reference.target).or_default().push(reference);
        }
        map
    }

    /// Linear search by id; prefer [`Self::index_by_id`] when looking up many.
    pub fn find(&self, id: &Oid) -> Option<&Commit> {
        self.commits.iter().find(|c| &c.id == id)
    }

    pub fn tag(&self, name: &str) -> Option<&Tag> {
        self.tags.iter().find(|t| t.name == name)
    }

    /// The commit HEAD points at; `None` when unborn or unknown.
    pub fn head_target(&self) -> Option<&Oid> {
        self.head.as_ref().and_then(Head::target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TagAnnotation;
    use crate::test_util::{oid, round_trip};

    fn commit(id: u8, parents: &[u8]) -> Commit {
        Commit {
            id: oid(id),
            parents: parents.iter().copied().map(oid).collect(),
            author_name: "Ada".into(),
            author_email: "ada@example.com".into(),
            author_time: 1_700_000_000,
            committer_time: -5,
            summary: "Fix: ünïcödé summary".into(),
            description: String::new(),
            lossy: false,
        }
    }

    #[test]
    fn description_preview_collapses_and_caps() {
        assert_eq!(Commit::description_preview(""), "");
        assert_eq!(Commit::description_preview("  line 1\n\n  line\t2 \n"), "line 1 line 2");
        let long = Commit::description_preview(&"é ".repeat(500));
        assert_eq!(long.chars().count(), Commit::DESCRIPTION_PREVIEW_CHARS);
    }

    fn wrapper() -> CommitWrapper {
        CommitWrapper {
            commits: vec![commit(3, &[2, 1]), commit(2, &[1]), commit(1, &[])].into(),
            references: vec![
                Reference::from_refname("refs/heads/main", oid(3)),
                Reference::from_refname("refs/remotes/origin/main", oid(2)),
                Reference::from_refname("refs/tags/v1", oid(3)),
            ]
            .into(),
            tags: vec![Tag {
                name: "v1".into(),
                target: oid(3),
                annotation: Some(TagAnnotation {
                    message: "release".into(),
                    tagger: None,
                }),
            }]
            .into(),
            head: Some(Head::Branch {
                name: "main".into(),
                refname: "refs/heads/main".into(),
                target: oid(3),
            }),
            partial: false,
        }
    }

    #[test]
    fn commit_shape() {
        let merge = commit(3, &[2, 1]);
        assert!(merge.is_merge() && !merge.is_root());
        assert_eq!(merge.first_parent(), Some(&oid(2)));
        let root = commit(1, &[]);
        assert!(root.is_root() && !root.is_merge());
        assert_eq!(root.first_parent(), None);
        assert_eq!(root.short_id(), "0101010");
    }

    #[test]
    fn wrapper_lookups() {
        let w = wrapper();
        assert_eq!(w.len(), 3);
        assert!(!w.is_empty() && CommitWrapper::default().is_empty());
        let index = w.index_by_id();
        assert_eq!(index.get(&oid(1)), Some(&2));
        assert_eq!(w.find(&oid(2)).map(|c| c.summary.as_str()), Some("Fix: ünïcödé summary"));
        assert!(w.find(&oid(9)).is_none());
        let by_target = w.references_by_target();
        let names: Vec<&str> = by_target[&oid(3)].iter().map(|r| r.short_name.as_str()).collect();
        assert_eq!(names, ["main", "v1"]);
        assert_eq!(by_target[&oid(2)].len(), 1);
        assert!(w.tag("v1").is_some() && w.tag("v2").is_none());
        assert_eq!(w.head_target(), Some(&oid(3)));
        assert_eq!(CommitWrapper::default().head_target(), None);
    }

    #[test]
    fn serde_round_trip() {
        round_trip(&commit(3, &[2, 1]));
        round_trip(&wrapper());
        round_trip(&CommitWrapper::default());
    }

    #[test]
    fn lossy_defaults_to_false_and_body_round_trips() -> Result<(), serde_json::Error> {
        let mut json = serde_json::to_value(commit(1, &[]))?;
        json.as_object_mut().map(|o| o.remove("lossy"));
        let parsed: Commit = serde_json::from_value(json)?;
        assert!(!parsed.lossy);
        round_trip(&CommitBody {
            message: "caf\u{fffd}\n".into(),
            lossy: true,
            encoding: Some("ISO-8859-1".into()),
        });
        Ok(())
    }
}
