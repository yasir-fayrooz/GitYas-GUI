//! WP9: `--mock` mode. Static data in core types (ported from the egui template's mock repository) so the UI and
//! its snapshots work without a repository on disk. The engine runs in memory with no session, so commands fail
//! with `NoActiveRepository` (shown as a toast).

use std::sync::Arc;

use gityas_core::models::{
    Branch, BranchKind, ChangeCounts, Commit, CommitSignature, CommitWrapper, Delta, DeltaChange, FileState, Head, OpenTabs, Reference,
    RepoStatus, Repository, Stash, StashDetails, Tag, TagAnnotation,
};
use gityas_core::{AppEvent, Oid, Provider};

use crate::app::GitYasApp;

pub const MOCK_REPO_ID: &str = "mock";

/// A deterministic 20-byte oid from `seed` (FNV-1a, expanded).
pub fn oid(seed: &str) -> Oid {
    let mut bytes = Vec::with_capacity(24);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for round in 0u8..3 {
        for b in seed.bytes().chain([round]) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        bytes.extend_from_slice(&h.to_be_bytes());
    }
    bytes.truncate(20);
    match Oid::from_bytes(&bytes) {
        Ok(oid) => oid,
        // `from_bytes` only rejects an empty slice.
        Err(_) => unreachable!("20-byte oid"),
    }
}

const JOHN: (&str, &str) = ("John Smith", "john.smith@example.com");
const ALEX: (&str, &str) = ("Alex Chen", "alex@example.com");
const SAM: (&str, &str) = ("Sam Rivera", "sam@example.com");

/// 2026-10-18 10:42 UTC.
const NEWEST: i64 = 1_792_320_120;
const HOUR: i64 = 3600;

fn commit(key: &str, parents: &[&str], summary: &str, (name, email): (&str, &str), age_hours: i64) -> Commit {
    let time = NEWEST - age_hours * HOUR;
    Commit {
        id: oid(key),
        parents: parents.iter().map(|p| oid(p)).collect(),
        author_name: name.into(),
        author_email: email.into(),
        author_time: time,
        committer_time: time,
        summary: summary.into(),
        description: String::new(),
        lossy: false,
    }
}

fn branch(name: &str, kind: BranchKind, key: &str, ahead: usize, behind: usize, is_head: bool) -> Branch {
    let refname = format!("{}{name}", kind.refname_prefix());
    let remote_name = (kind == BranchKind::Remote).then(|| name.split('/').next().unwrap_or(name).to_owned());
    let upstream = (kind == BranchKind::Local && name != "feature/hunk-perf").then(|| format!("origin/{name}"));
    Branch {
        name: name.into(),
        upstream_refname: upstream.as_ref().map(|u| format!("refs/remotes/{u}")),
        upstream,
        refname,
        kind,
        target: Some(oid(key)),
        remote_name,
        upstream_gone: false,
        ahead,
        behind,
        is_head,
    }
}

/// The mock history: newest first, topologically ordered, with merges and 120 older linear commits.
pub fn commits() -> CommitWrapper {
    let mut commits = vec![
        commit("hunk", &["bump"], "wip: speed up hunk staging for large diffs", SAM, 0),
        commit(
            "merge-egui",
            &["fix-store", "graph"],
            "Merge branch 'feature/egui-frontend'",
            JOHN,
            16,
        ),
        commit("graph", &["scaffold"], "feat: commit graph", JOHN, 18),
        commit("fix-store", &["bump"], "fix: store not persisting window size", ALEX, 23),
        commit("scaffold", &["merge-tags"], "feat: scaffold egui app", JOHN, 36),
        commit("bump", &["merge-tags"], "chore: bump version to 2.5", ALEX, 44),
        commit(
            "merge-tags",
            &["readme", "tags-remote"],
            "Merge pull request #42 from feature/tags",
            JOHN,
            65,
        ),
        commit("readme", &["store"], "docs: update README", SAM, 69),
        commit("tags-remote", &["tags-local"], "feat: delete remote tags", JOHN, 72),
        commit("tags-local", &["store"], "feat: create local tags", JOHN, 86),
        commit("store", &["providers"], "feat: added store for data persistence", JOHN, 89),
        commit("providers", &["tags-feature"], "feat: bitbucket, ado, gitlab support", JOHN, 187),
        commit(
            "tags-feature",
            &["hunks"],
            "feat: tags feature, local, remote tag creating & deleting",
            JOHN,
            255,
        ),
        commit("hunks", &["ctx-menu"], "feat: stage, unstage, discard hunks", JOHN, 359),
        commit("ctx-menu", &["base"], "feat: context menu staged/unstaged files", JOHN, 446),
    ];
    let older = [
        "refactor: split data-access into git_rs module",
        "feat: file watcher with debounce",
        "fix: crash when opening bare repo",
        "feat: diff viewer with monaco",
        "chore: add prettier config",
        "feat: oauth login for github",
        "feat: clone repository dialog",
        "fix: graph lanes overlapping on octopus merges",
    ];
    let total_older = 120;
    for i in 0..total_older {
        let key = if i == 0 { "base".to_string() } else { format!("old-{i}") };
        let parent = format!("old-{}", i + 1);
        let parents: Vec<&str> = if i + 1 < total_older { vec![parent.as_str()] } else { vec![] };
        let author = if i % 3 == 0 { ALEX } else { JOHN };
        commits.push(commit(&key, &parents, older[i % older.len()], author, 460 + i as i64 * 5));
    }
    let tag = |name: &str, key: &str, message: Option<&str>| Tag {
        name: name.into(),
        target: oid(key),
        annotation: message.map(|m| TagAnnotation {
            message: m.into(),
            tagger: Some(CommitSignature::new(JOHN.0, JOHN.1)),
        }),
    };
    let references = vec![
        Reference::from_refname("refs/heads/main", oid("merge-egui")),
        Reference::from_refname("refs/heads/feature/egui-frontend", oid("graph")),
        Reference::from_refname("refs/heads/feature/hunk-perf", oid("hunk")),
        Reference::from_refname("refs/remotes/origin/main", oid("merge-egui")),
        Reference::from_refname("refs/remotes/origin/HEAD", oid("merge-egui")),
        Reference::from_refname("refs/remotes/origin/feature/egui-frontend", oid("graph")),
        Reference::from_refname("refs/remotes/origin/feature/tags", oid("tags-remote")),
        Reference::from_refname("refs/tags/v0.3.0", oid("merge-tags")),
        Reference::from_refname("refs/tags/v0.2.0", oid("providers")),
        Reference::from_refname("refs/tags/v0.1.0", oid("ctx-menu")),
    ];
    CommitWrapper {
        commits: commits.into(),
        references: references.into(),
        tags: Arc::from(vec![
            tag("v0.3.0", "merge-tags", Some("Release 0.3.0")),
            tag("v0.2.0", "providers", None),
            tag("v0.1.0", "ctx-menu", None),
        ]),
        head: Some(head()),
        partial: false,
    }
}

pub fn head() -> Head {
    Head::Branch {
        name: "main".into(),
        refname: "refs/heads/main".into(),
        target: oid("merge-egui"),
    }
}

pub fn files() -> FileState {
    FileState {
        unstaged: vec![
            Delta::new("src-frontend/src/app.rs", DeltaChange::Modified),
            Delta::new("src-frontend/src/ui/sidebar.rs", DeltaChange::Untracked),
            Delta::new("src/components/side-bar/old-sidebar.tsx", DeltaChange::Deleted),
            Delta::moved(
                "src-frontend/src/ui/graph.rs",
                "src-frontend/src/ui/graph_view.rs",
                DeltaChange::Renamed,
            ),
        ],
        staged: vec![Delta::new("src-frontend/src/theme.rs", DeltaChange::Modified)],
    }
}

pub fn stashes() -> Vec<StashDetails> {
    let stash = |index: usize, key: &str, message: &str, files: Vec<Delta>| StashDetails {
        stash: Stash {
            index,
            id: oid(key),
            message: message.into(),
            author: CommitSignature::new(JOHN.0, JOHN.1),
            time: NEWEST - (index as i64 + 1) * 30 * HOUR,
            counts: ChangeCounts::from_deltas(&files),
        },
        files,
    };
    vec![
        stash(
            0,
            "stash-0",
            "WIP on main: experiment with libgit2 blame",
            vec![Delta::new("src-backend/gityas-git/src/blame.rs", DeltaChange::Added)],
        ),
        stash(
            1,
            "stash-1",
            "On feature/tags: half-finished tag dialog",
            vec![
                Delta::new("src/components/side-bar/tags.tsx", DeltaChange::Modified),
                Delta::new("src/components/tag-dialog.tsx", DeltaChange::Added),
            ],
        ),
    ]
}

pub fn repository() -> Repository {
    Repository {
        id: MOCK_REPO_ID.into(),
        name: "git-yas".into(),
        path: "/mock/git-yas".into(),
        provider: Some(Provider::Github),
        sort_order: 0,
    }
}

/// Fills the store as if the engine had opened the mock repository.
pub fn install(app: &mut GitYasApp) {
    let id = MOCK_REPO_ID.to_string();
    let local = vec![
        branch("main", BranchKind::Local, "merge-egui", 0, 0, true),
        branch("feature/egui-frontend", BranchKind::Local, "graph", 0, 0, false),
        branch("feature/hunk-perf", BranchKind::Local, "hunk", 1, 0, false),
    ];
    let remote = vec![
        branch("origin/main", BranchKind::Remote, "merge-egui", 0, 0, false),
        branch("origin/feature/egui-frontend", BranchKind::Remote, "graph", 0, 0, false),
        branch("origin/feature/tags", BranchKind::Remote, "tags-remote", 0, 0, false),
    ];
    let events = [
        AppEvent::RepositoriesUpdated(vec![repository()]),
        AppEvent::SelectedRepositoryUpdated(Some(id.clone())),
        AppEvent::CommitsUpdated {
            repo: id.clone(),
            commits: commits(),
        },
        AppEvent::CommitsHeadUpdated {
            repo: id.clone(),
            head: Some(head()),
        },
        AppEvent::LocalBranchesUpdated {
            repo: id.clone(),
            branches: local,
        },
        AppEvent::RemoteBranchesUpdated {
            repo: id.clone(),
            branches: remote,
        },
        AppEvent::RemotesUpdated {
            repo: id.clone(),
            remotes: vec!["origin".into()],
        },
        AppEvent::GitFilesChanged {
            repo: id.clone(),
            files: files(),
        },
        AppEvent::StashesUpdated {
            repo: id.clone(),
            stashes: stashes(),
        },
        AppEvent::RepoStatusUpdated {
            repo: id.clone(),
            status: RepoStatus::default(),
        },
        AppEvent::CommitsSignatureUpdated(CommitSignature::new(JOHN.0, JOHN.1)),
    ];
    app.store.open_tabs = OpenTabs {
        open: vec![id.clone()],
        active: Some(id),
    };
    for event in events {
        app.handle_event(event);
    }
    app.store.auth = Provider::ALL.iter().map(|p| (*p, *p == Provider::Github)).collect();
    app.store.started = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_topological_and_parents_exist() {
        let wrapper = commits();
        let index = wrapper.index_by_id();
        for (i, c) in wrapper.commits.iter().enumerate() {
            for parent in &c.parents {
                let pi = index
                    .get(parent)
                    .copied()
                    .unwrap_or_else(|| panic!("missing parent of {}", c.summary));
                assert!(pi > i, "{} must come before its parent", c.summary);
            }
        }
        assert_eq!(wrapper.len(), 135);
        assert_ne!(oid("a"), oid("b"));
    }
}
