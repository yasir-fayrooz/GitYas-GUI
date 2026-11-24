//! Engine-derived state, updated only by [`Store::apply`] (events) and a few command results.
//!
//! The UI never mutates this optimistically: it calls an engine command and waits for the events. Data is per
//! *active* repository; switching tabs replaces [`Store::repo`] and the engine re-emits everything.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gityas_core::models::{
    Branch, Commit, CommitSignature, CommitWrapper, FileState, Head, OpenTabs, Reference, RemoteProgress, RepoStatus, Repository, Settings,
    StashDetails, Worktree,
};
use gityas_core::{AppEvent, Oid, Provider, RepoId};

use crate::graph::{self, GraphLayout};

/// Which parts of [`RepoData`] have arrived since the repository was opened (loading spinners).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Loaded {
    pub commits: bool,
    pub head: bool,
    pub local_branches: bool,
    pub remote_branches: bool,
    pub remotes: bool,
    pub files: bool,
    pub stashes: bool,
    pub status: bool,
    pub worktrees: bool,
}

/// Everything known about the active repository.
#[derive(Debug, Clone, Default)]
pub struct RepoData {
    pub id: RepoId,
    /// The history as shown: the engine's, cut to `GraphOptions::max_commits` and to the [`RepoData::solo`] branch
    /// (see [`RepoData::limit_commits`]).
    pub commits: CommitWrapper,
    /// Every commit the engine sent (`commits.commits` is a subsequence of it; a prefix when not soloing).
    pub all_commits: Arc<[Commit]>,
    /// Full ref name of the soloed branch: only its tip and ancestors are shown. Cleared when the ref disappears.
    pub solo: Option<String>,
    /// `None` until `CommitsHeadUpdated` arrives.
    pub head: Option<Head>,
    pub local_branches: Vec<Branch>,
    pub remote_branches: Vec<Branch>,
    pub remotes: Vec<String>,
    pub files: FileState,
    pub stashes: Vec<StashDetails>,
    /// Every worktree of the repository (main first), from `WorktreesUpdated`.
    pub worktrees: Vec<Worktree>,
    pub status: RepoStatus,
    pub loaded: Loaded,
    /// Derived from `commits` + `files` (WIP row) on every change; see [`RepoData::rebuild`].
    pub graph: Arc<GraphLayout>,
    /// Commit index by id (derived).
    pub index: Arc<HashMap<Oid, usize>>,
    /// References grouped by target commit (derived; HEAD's branch first, then local, remote, tags).
    pub refs_by_commit: Arc<HashMap<Oid, Vec<Reference>>>,
}

impl RepoData {
    pub fn new(id: RepoId) -> Self {
        Self { id, ..Self::default() }
    }

    /// True once the history and the head have arrived (the graph can be drawn).
    pub fn is_ready(&self) -> bool {
        self.loaded.commits && self.loaded.head
    }

    pub fn has_wip(&self) -> bool {
        !self.files.is_clean()
    }

    /// The commit HEAD points at, if any.
    pub fn head_target(&self) -> Option<&Oid> {
        self.head.as_ref().and_then(Head::target).or_else(|| self.commits.head_target())
    }

    pub fn head_branch(&self) -> Option<&Branch> {
        self.local_branches.iter().find(|b| b.is_head)
    }

    pub fn commit_index(&self, id: &Oid) -> Option<usize> {
        self.index.get(id).copied()
    }

    /// Shows the newest `max` commits of [`RepoData::all_commits`] (at least one), or when soloing the newest `max`
    /// reachable from the soloed branch. Returns whether that changed the shown list; the caller then
    /// [`rebuild`](RepoData::rebuild)s.
    fn limit_commits(&mut self, max: usize) -> bool {
        let max = max.max(1);
        let tip = self.solo.as_deref().and_then(|refname| {
            let reference = self.commits.references.iter().find(|r| r.name == refname)?;
            Some(reference.target.clone())
        });
        if tip.is_none() {
            self.solo = None;
        }
        let shown: Arc<[Commit]> = match tip {
            Some(tip) => reachable(&self.all_commits, tip, max).into(),
            None if self.all_commits.len() <= max => Arc::clone(&self.all_commits),
            None => self.all_commits[..max].into(),
        };
        if Arc::ptr_eq(&shown, &self.commits.commits) || shown[..] == self.commits.commits[..] {
            return false;
        }
        self.commits.commits = shown;
        true
    }

    /// Recomputes the derived fields (graph layout, lookup maps).
    pub fn rebuild(&mut self) {
        let wip_parent = self.head_target().filter(|_| self.has_wip()).cloned();
        self.graph = Arc::new(graph::layout(&self.commits.commits, wip_parent.as_ref()));
        self.index = Arc::new(self.commits.commits.iter().enumerate().map(|(i, c)| (c.id.clone(), i)).collect());
        let head_ref = self.head.as_ref().and_then(Head::refname).map(str::to_owned);
        let mut by_commit: HashMap<Oid, Vec<Reference>> = HashMap::new();
        for reference in self.commits.references.iter() {
            if reference.is_remote_head() || !(reference.is_branch() || reference.is_tag()) {
                continue;
            }
            by_commit.entry(reference.target.clone()).or_default().push(reference.clone());
        }
        for refs in by_commit.values_mut() {
            refs.sort_by_key(|r| {
                let rank = if head_ref.as_deref() == Some(r.name.as_str()) {
                    0
                } else {
                    match r.kind {
                        gityas_core::models::ReferenceKind::Branch(gityas_core::models::BranchKind::Local) => 1,
                        gityas_core::models::ReferenceKind::Branch(gityas_core::models::BranchKind::Remote) => 2,
                        _ => 3,
                    }
                };
                (rank, r.short_name.clone())
            });
        }
        self.refs_by_commit = Arc::new(by_commit);
    }
}

/// The first `max` commits of `commits` (children before parents) that `tip` reaches, in order. One pass: a commit
/// is reachable if it is the tip or a parent of a reachable commit listed above it.
fn reachable(commits: &[Commit], tip: Oid, max: usize) -> Vec<Commit> {
    let mut wanted = HashSet::from([tip]);
    let mut out = Vec::new();
    for commit in commits {
        if out.len() == max {
            break;
        }
        if wanted.remove(&commit.id) {
            wanted.extend(commit.parents.iter().cloned());
            out.push(commit.clone());
        }
    }
    out
}

/// Repositories whose data the store keeps after switching away (the engine keeps as many histories in memory).
pub const RECENT_REPOS: usize = 6;

#[derive(Debug, Default)]
pub struct Store {
    /// `Engine::start` finished (the loading frame is replaced by the normal UI).
    pub started: bool,
    /// `Engine::start` (or engine construction) failed; shown instead of the UI.
    pub fatal: Option<String>,
    pub repositories: Vec<Repository>,
    /// Tab order + active tab as persisted by the engine (`open_tabs` / `set_open_tabs`). The repos UI keeps it in sync.
    pub open_tabs: OpenTabs,
    /// The engine's active repository (`SelectedRepositoryUpdated`).
    pub active: Option<RepoId>,
    /// Data of the active repository; `None` when no repository is open.
    pub repo: Option<RepoData>,
    /// Data of recently active repositories, most recent first (at most [`RECENT_REPOS`]). When the engine switches
    /// back to one of them it is shown at once, slightly stale, while the engine's refresh (from its in-memory
    /// history) brings it up to date.
    recent: Vec<RepoData>,
    pub settings: Settings,
    /// Connection state per provider (`Provider::ALL` order once `auth_status` / events arrived).
    pub auth: Vec<(Provider, bool)>,
    /// Latest progress of the running remote operation (cleared by its completion).
    pub progress: Option<(Option<RepoId>, RemoteProgress)>,
}

impl Store {
    pub fn repository(&self, id: &str) -> Option<&Repository> {
        self.repositories.iter().find(|r| r.id == id)
    }

    pub fn active_repository(&self) -> Option<&Repository> {
        self.active.as_deref().and_then(|id| self.repository(id))
    }

    pub fn signature(&self) -> Option<&CommitSignature> {
        self.settings.commit_signature.as_ref().filter(|s| s.is_valid())
    }

    pub fn is_connected(&self, provider: Provider) -> bool {
        self.auth.iter().any(|(p, c)| *p == provider && *c)
    }

    /// Applies one engine event. Repo-scoped events for anything but the active repository are dropped (a late
    /// event from the previous tab must never paint into the new one). Returns `false` if it was dropped.
    pub fn apply(&mut self, event: AppEvent) -> bool {
        if !event.is_for(self.active.as_deref()) {
            tracing::debug!(event = event.name(), "dropping event for an inactive repository");
            return false;
        }
        match event {
            AppEvent::RepositoriesUpdated(repos) => {
                self.repositories = repos;
                let repositories = &self.repositories;
                self.recent.retain(|r| repositories.iter().any(|known| known.id == r.id));
                // Drop tabs of removed repositories. Keep "no active tab" as it is (like the engine's own
                // `normalize_tabs`): `OpenTabs::normalize` would make the first tab active with no session running.
                let known: Vec<&str> = self.repositories.iter().map(|r| r.id.as_str()).collect();
                let no_active = self.open_tabs.active.is_none();
                self.open_tabs.normalize(&known);
                if no_active {
                    self.open_tabs.active = None;
                }
            }
            AppEvent::SelectedRepositoryUpdated(id) => {
                if self.repo.as_ref().map(|r| &r.id) != id.as_ref() {
                    if let Some(previous) = self.repo.take().filter(RepoData::is_ready) {
                        self.recent.retain(|r| r.id != previous.id);
                        self.recent.insert(0, previous);
                        self.recent.truncate(RECENT_REPOS);
                    }
                    let max = self.settings.graph.max_commits;
                    self.repo = id.clone().map(|id| match self.recent.iter().position(|r| r.id == id) {
                        Some(at) => {
                            let mut restored = self.recent.remove(at);
                            if restored.limit_commits(max) {
                                restored.rebuild();
                            }
                            restored
                        }
                        None => RepoData::new(id),
                    });
                }
                self.active = id;
            }
            AppEvent::CommitsUpdated { commits, .. } => {
                let max = self.settings.graph.max_commits;
                self.with_repo(|r| {
                    r.all_commits = Arc::clone(&commits.commits);
                    r.commits = commits;
                    r.limit_commits(max);
                    r.loaded.commits = true;
                    r.rebuild();
                })
            }
            AppEvent::CommitsHeadUpdated { head, .. } => self.with_repo(|r| {
                r.head = head;
                r.loaded.head = true;
                r.rebuild();
            }),
            AppEvent::LocalBranchesUpdated { branches, .. } => self.with_repo(|r| {
                r.local_branches = branches;
                r.loaded.local_branches = true;
            }),
            AppEvent::RemoteBranchesUpdated { branches, .. } => self.with_repo(|r| {
                r.remote_branches = branches;
                r.loaded.remote_branches = true;
            }),
            AppEvent::RemotesUpdated { remotes, .. } => self.with_repo(|r| {
                r.remotes = remotes;
                r.loaded.remotes = true;
            }),
            AppEvent::GitFilesChanged { files, .. } => self.with_repo(|r| {
                let was_dirty = r.has_wip();
                r.files = files;
                r.loaded.files = true;
                if was_dirty != r.has_wip() {
                    r.rebuild();
                }
            }),
            AppEvent::StashesUpdated { stashes, .. } => self.with_repo(|r| {
                r.stashes = stashes;
                r.loaded.stashes = true;
            }),
            AppEvent::WorktreesUpdated { worktrees, .. } => self.with_repo(|r| {
                r.worktrees = worktrees;
                r.loaded.worktrees = true;
            }),
            AppEvent::RepoStatusUpdated { status, .. } => self.with_repo(|r| {
                r.status = status;
                r.loaded.status = true;
            }),
            AppEvent::CommitsSignatureUpdated(signature) => self.settings.commit_signature = Some(signature),
            AppEvent::AuthStatusUpdated { provider, connected } => match self.auth.iter_mut().find(|(p, _)| *p == provider) {
                Some(entry) => entry.1 = connected,
                None => self.auth.push((provider, connected)),
            },
            AppEvent::RemoteProgress { repo, progress } => self.progress = Some((repo, progress)),
            // Shown by `GitYasApp::handle_event` (banner + notification log); nothing to store.
            AppEvent::Warning(_) => {}
        }
        true
    }

    /// Replaces the settings and re-cuts the shown history if `GraphOptions::max_commits` changed.
    pub fn set_settings(&mut self, settings: Settings) {
        let max = settings.graph.max_commits;
        self.settings = settings;
        self.with_repo(|r| {
            if r.limit_commits(max) {
                r.rebuild();
            }
        });
    }

    /// Shows only `refname` and its ancestors (`None` = every branch).
    pub fn set_solo(&mut self, refname: Option<String>) {
        let max = self.settings.graph.max_commits;
        self.with_repo(|r| {
            r.solo = refname;
            if r.limit_commits(max) {
                r.rebuild();
            }
        });
    }

    fn with_repo(&mut self, f: impl FnOnce(&mut RepoData)) {
        if let Some(repo) = self.repo.as_mut() {
            f(repo);
        }
    }
}

#[cfg(test)]
mod tests {
    use gityas_core::models::DeltaChange;

    use super::*;

    fn oid(b: u8) -> Oid {
        Oid::from_bytes(&[b; 20]).unwrap_or_else(|e| panic!("{e}"))
    }

    fn commit(id: u8, parents: &[u8]) -> Commit {
        Commit {
            id: oid(id),
            parents: parents.iter().copied().map(oid).collect(),
            author_name: "A".into(),
            author_email: "a@x".into(),
            author_time: 0,
            committer_time: 0,
            summary: format!("c{id}"),
            description: String::new(),
            lossy: false,
        }
    }

    fn opened(store: &mut Store, id: &str) {
        store.apply(AppEvent::SelectedRepositoryUpdated(Some(id.into())));
    }

    #[test]
    fn history_is_cut_to_max_commits() {
        let mut store = Store::default();
        let mut settings = Settings::default();
        settings.graph.max_commits = 2;
        store.set_settings(settings.clone());
        opened(&mut store, "a");
        store.apply(AppEvent::CommitsUpdated {
            repo: "a".into(),
            commits: CommitWrapper {
                commits: vec![commit(3, &[2]), commit(2, &[1]), commit(1, &[])].into(),
                ..CommitWrapper::default()
            },
        });
        let shown = |s: &Store| {
            s.repo
                .as_ref()
                .map(|r| (r.commits.len(), r.graph.rows.len(), r.commit_index(&oid(1))))
        };
        assert_eq!(shown(&store), Some((2, 2, None)));
        settings.graph.max_commits = 10;
        store.set_settings(settings.clone());
        assert_eq!(shown(&store), Some((3, 3, Some(2))));
        settings.graph.max_commits = 0;
        store.set_settings(settings);
        assert_eq!(shown(&store).map(|s| s.0), Some(1), "always at least one");
    }

    /// A repository whose history and head have arrived (ready to draw).
    fn loaded(store: &mut Store, id: &str, commits: Vec<Commit>) {
        opened(store, id);
        store.apply(AppEvent::CommitsUpdated {
            repo: id.into(),
            commits: CommitWrapper {
                commits: commits.into(),
                ..CommitWrapper::default()
            },
        });
        store.apply(AppEvent::CommitsHeadUpdated {
            repo: id.into(),
            head: Some(Head::Detached { target: oid(1) }),
        });
    }

    fn known(ids: &[&str]) -> AppEvent {
        AppEvent::RepositoriesUpdated(
            ids.iter()
                .map(|id| Repository {
                    id: (*id).into(),
                    name: (*id).into(),
                    path: std::path::PathBuf::from(id),
                    provider: None,
                    sort_order: 0,
                })
                .collect(),
        )
    }

    #[test]
    fn switching_back_restores_the_previous_data_at_once() {
        let mut store = Store::default();
        store.apply(known(&["a", "b"]));
        loaded(&mut store, "a", vec![commit(2, &[1]), commit(1, &[])]);
        let a_graph = store.repo.as_ref().map(|r| Arc::clone(&r.graph));
        loaded(&mut store, "b", vec![commit(9, &[])]);

        opened(&mut store, "a");
        let repo = store.repo.as_ref().unwrap_or_else(|| panic!("a is open"));
        assert!(repo.is_ready(), "drawn before the engine's refresh arrives");
        assert_eq!(repo.commits.commits.len(), 2);
        assert!(
            a_graph.is_some_and(|g| Arc::ptr_eq(&g, &repo.graph)),
            "layout reused, not recomputed"
        );

        // Events for the restored repository update it as usual.
        store.apply(AppEvent::CommitsUpdated {
            repo: "a".into(),
            commits: CommitWrapper {
                commits: vec![commit(3, &[2]), commit(2, &[1]), commit(1, &[])].into(),
                ..CommitWrapper::default()
            },
        });
        assert_eq!(store.repo.as_ref().map(|r| r.commits.commits.len()), Some(3));
        // And b is still remembered.
        opened(&mut store, "b");
        assert_eq!(
            store.repo.as_ref().map(|r| (r.is_ready(), r.commits.commits.len())),
            Some((true, 1))
        );
    }

    #[test]
    fn restored_data_follows_settings_and_removed_repositories_are_forgotten() {
        let mut store = Store::default();
        store.apply(known(&["a", "b"]));
        loaded(&mut store, "a", vec![commit(3, &[2]), commit(2, &[1]), commit(1, &[])]);
        loaded(&mut store, "b", vec![commit(9, &[])]);
        let mut settings = Settings::default();
        settings.graph.max_commits = 1;
        store.set_settings(settings);
        opened(&mut store, "a");
        assert_eq!(
            store.repo.as_ref().map(|r| r.commits.commits.len()),
            Some(1),
            "re-cut to the new maximum"
        );

        store.apply(known(&["a"]));
        opened(&mut store, "b");
        assert!(
            !store.repo.as_ref().is_some_and(RepoData::is_ready),
            "b was removed: nothing to restore"
        );
    }

    #[test]
    fn only_the_most_recent_repositories_are_kept() {
        let mut store = Store::default();
        let ids: Vec<String> = (0..=RECENT_REPOS + 1).map(|i| format!("r{i}")).collect();
        store.apply(known(&ids.iter().map(String::as_str).collect::<Vec<_>>()));
        for id in &ids {
            loaded(&mut store, id, vec![commit(1, &[])]);
        }
        opened(&mut store, &ids[0]);
        assert!(!store.repo.as_ref().is_some_and(RepoData::is_ready), "the oldest was evicted");
        opened(&mut store, &ids[2]);
        assert!(store.repo.as_ref().is_some_and(RepoData::is_ready));
    }

    #[test]
    fn solo_shows_only_the_branch_and_its_ancestors() {
        let mut store = Store::default();
        opened(&mut store, "a");
        // 4 (main) -> 2 -> 1, 3 (topic) -> 1.
        store.apply(AppEvent::CommitsUpdated {
            repo: "a".into(),
            commits: CommitWrapper {
                commits: vec![commit(4, &[2]), commit(3, &[1]), commit(2, &[1]), commit(1, &[])].into(),
                references: vec![
                    Reference::from_refname("refs/heads/main", oid(4)),
                    Reference::from_refname("refs/heads/topic", oid(3)),
                ]
                .into(),
                ..CommitWrapper::default()
            },
        });
        let shown = |s: &Store| {
            s.repo
                .as_ref()
                .map(|r| r.commits.commits.iter().map(|c| c.id.clone()).collect::<Vec<_>>())
        };
        store.set_solo(Some("refs/heads/topic".into()));
        assert_eq!(shown(&store), Some(vec![oid(3), oid(1)]));
        assert!(
            store
                .repo
                .as_ref()
                .is_some_and(|r| r.graph.rows.len() == 2 && r.commit_index(&oid(2)).is_none())
        );
        store.set_solo(None);
        assert_eq!(shown(&store).map(|s| s.len()), Some(4));

        // A soloed branch that is deleted ends the solo.
        store.set_solo(Some("refs/heads/topic".into()));
        store.apply(AppEvent::CommitsUpdated {
            repo: "a".into(),
            commits: CommitWrapper {
                commits: vec![commit(4, &[2]), commit(2, &[1]), commit(1, &[])].into(),
                references: vec![Reference::from_refname("refs/heads/main", oid(4))].into(),
                ..CommitWrapper::default()
            },
        });
        assert!(
            store
                .repo
                .as_ref()
                .is_some_and(|r| r.solo.is_none() && r.commits.commits.len() == 3)
        );
    }

    #[test]
    fn drops_events_for_other_repositories() {
        let mut store = Store::default();
        opened(&mut store, "a");
        let stale = AppEvent::RemotesUpdated {
            repo: "b".into(),
            remotes: vec!["origin".into()],
        };
        assert!(!store.apply(stale));
        assert!(store.repo.as_ref().is_some_and(|r| r.remotes.is_empty() && !r.loaded.remotes));
        assert!(store.apply(AppEvent::RemotesUpdated {
            repo: "a".into(),
            remotes: vec!["origin".into()],
        }));
        assert_eq!(store.repo.as_ref().map(|r| r.remotes.len()), Some(1));
    }

    #[test]
    fn switching_resets_repo_data_and_reselecting_keeps_it() {
        let mut store = Store::default();
        opened(&mut store, "a");
        store.apply(AppEvent::RemotesUpdated {
            repo: "a".into(),
            remotes: vec!["origin".into()],
        });
        opened(&mut store, "a");
        assert!(store.repo.as_ref().is_some_and(|r| r.loaded.remotes));
        opened(&mut store, "b");
        assert!(store.repo.as_ref().is_some_and(|r| r.id == "b" && !r.loaded.remotes));
        store.apply(AppEvent::SelectedRepositoryUpdated(None));
        assert!(store.repo.is_none() && store.active.is_none());
    }

    #[test]
    fn repositories_update_keeps_no_active_tab() {
        let mut store = Store {
            open_tabs: OpenTabs {
                open: vec!["a".into(), "gone".into()],
                active: None,
            },
            ..Store::default()
        };
        let repo = |id: &str| Repository {
            id: id.into(),
            name: id.into(),
            path: format!("/{id}").into(),
            provider: None,
            sort_order: 0,
        };
        store.apply(AppEvent::RepositoriesUpdated(vec![repo("a")]));
        assert_eq!(store.open_tabs.open, ["a"]);
        assert_eq!(store.open_tabs.active, None);
    }

    #[test]
    fn graph_gets_a_wip_row_when_the_tree_is_dirty() {
        let mut store = Store::default();
        opened(&mut store, "a");
        store.apply(AppEvent::CommitsUpdated {
            repo: "a".into(),
            commits: CommitWrapper {
                commits: vec![commit(2, &[1]), commit(1, &[])].into(),
                references: vec![Reference::from_refname("refs/heads/main", oid(2))].into(),
                tags: Arc::from(vec![]),
                head: None,
                partial: false,
            },
        });
        store.apply(AppEvent::CommitsHeadUpdated {
            repo: "a".into(),
            head: Some(Head::Branch {
                name: "main".into(),
                refname: "refs/heads/main".into(),
                target: oid(2),
            }),
        });
        let repo = store.repo.as_ref().unwrap_or_else(|| panic!("repo"));
        assert!(repo.is_ready() && repo.graph.wip.is_none());
        assert_eq!(repo.commit_index(&oid(1)), Some(1));
        assert_eq!(repo.refs_by_commit.get(&oid(2)).map(Vec::len), Some(1));

        store.apply(AppEvent::GitFilesChanged {
            repo: "a".into(),
            files: FileState {
                unstaged: vec![gityas_core::models::Delta::new("f.txt", DeltaChange::Modified)],
                staged: vec![],
            },
        });
        assert!(store.repo.as_ref().is_some_and(|r| r.graph.wip.is_some()));
    }
}
