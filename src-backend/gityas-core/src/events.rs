//! Typed events pushed from the engine to the UI (replaces the old string event names + JSON payloads).

use std::sync::Mutex;

use crate::models::{
    Branch, CommitSignature, CommitWrapper, FileState, Head, RemoteProgress, RepoStatus, Repository, StashDetails, Worktree,
};
use crate::{Provider, RepoId};

/// Repo-scoped variants carry `repo` so the frontend can drop late events from a previously active repository.
#[derive(Debug, Clone, PartialEq)]
pub enum AppEvent {
    RepositoriesUpdated(Vec<Repository>),
    SelectedRepositoryUpdated(Option<RepoId>),
    CommitsUpdated {
        repo: RepoId,
        commits: CommitWrapper,
    },
    CommitsHeadUpdated {
        repo: RepoId,
        head: Option<Head>,
    },
    LocalBranchesUpdated {
        repo: RepoId,
        branches: Vec<Branch>,
    },
    RemoteBranchesUpdated {
        repo: RepoId,
        branches: Vec<Branch>,
    },
    RemotesUpdated {
        repo: RepoId,
        remotes: Vec<String>,
    },
    GitFilesChanged {
        repo: RepoId,
        files: FileState,
    },
    /// Stash list, newest first (`stash@{0}` first), with each stash's files.
    StashesUpdated {
        repo: RepoId,
        stashes: Vec<StashDetails>,
    },
    /// Every worktree of the repository (`git worktree list`), the main one first.
    WorktreesUpdated {
        repo: RepoId,
        worktrees: Vec<Worktree>,
    },
    /// Operation in progress (merge, rebase, ...) and repository caveats (shallow, bare).
    RepoStatusUpdated {
        repo: RepoId,
        status: RepoStatus,
    },
    CommitsSignatureUpdated(CommitSignature),
    AuthStatusUpdated {
        provider: Provider,
        connected: bool,
    },
    /// Non-fatal problem worth showing, e.g. a corrupt user DB was recovered.
    Warning(String),
    /// Progress of a fetch / pull / push / clone. `repo` is the session's repository, `None` for a clone (the
    /// repository isn't registered yet).
    RemoteProgress {
        repo: Option<RepoId>,
        progress: RemoteProgress,
    },
}

impl AppEvent {
    /// The repository a repo-scoped event belongs to; `None` for global events.
    pub fn repo(&self) -> Option<&RepoId> {
        match self {
            AppEvent::CommitsUpdated { repo, .. }
            | AppEvent::CommitsHeadUpdated { repo, .. }
            | AppEvent::LocalBranchesUpdated { repo, .. }
            | AppEvent::RemoteBranchesUpdated { repo, .. }
            | AppEvent::RemotesUpdated { repo, .. }
            | AppEvent::GitFilesChanged { repo, .. }
            | AppEvent::StashesUpdated { repo, .. }
            | AppEvent::WorktreesUpdated { repo, .. }
            | AppEvent::RepoStatusUpdated { repo, .. } => Some(repo),
            AppEvent::RemoteProgress { repo, .. } => repo.as_ref(),
            AppEvent::RepositoriesUpdated(_)
            | AppEvent::SelectedRepositoryUpdated(_)
            | AppEvent::CommitsSignatureUpdated(_)
            | AppEvent::AuthStatusUpdated { .. }
            | AppEvent::Warning(_) => None,
        }
    }

    /// Variant name, for tracing (payloads can be large, so they are not logged).
    pub fn name(&self) -> &'static str {
        match self {
            AppEvent::RepositoriesUpdated(_) => "RepositoriesUpdated",
            AppEvent::SelectedRepositoryUpdated(_) => "SelectedRepositoryUpdated",
            AppEvent::CommitsUpdated { .. } => "CommitsUpdated",
            AppEvent::CommitsHeadUpdated { .. } => "CommitsHeadUpdated",
            AppEvent::LocalBranchesUpdated { .. } => "LocalBranchesUpdated",
            AppEvent::RemoteBranchesUpdated { .. } => "RemoteBranchesUpdated",
            AppEvent::RemotesUpdated { .. } => "RemotesUpdated",
            AppEvent::GitFilesChanged { .. } => "GitFilesChanged",
            AppEvent::StashesUpdated { .. } => "StashesUpdated",
            AppEvent::WorktreesUpdated { .. } => "WorktreesUpdated",
            AppEvent::RepoStatusUpdated { .. } => "RepoStatusUpdated",
            AppEvent::CommitsSignatureUpdated(_) => "CommitsSignatureUpdated",
            AppEvent::AuthStatusUpdated { .. } => "AuthStatusUpdated",
            AppEvent::Warning(_) => "Warning",
            AppEvent::RemoteProgress { .. } => "RemoteProgress",
        }
    }

    /// True if this event should be applied while `active` is the active repository: global events always,
    /// repo-scoped ones only when they belong to `active` (drops late events from a previously active repo).
    pub fn is_for(&self, active: Option<&str>) -> bool {
        match self.repo() {
            None => true,
            Some(repo) => active == Some(repo.as_str()),
        }
    }
}

/// Receives engine events. The frontend implementation pushes into a channel and requests a repaint.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: AppEvent);
}

/// Discards all events.
#[derive(Debug, Default)]
pub struct NoopSink;

impl EventSink for NoopSink {
    fn emit(&self, _event: AppEvent) {}
}

/// Records events for assertions in tests.
#[derive(Debug, Default)]
pub struct RecordingSink {
    events: Mutex<Vec<AppEvent>>,
}

impl RecordingSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns and clears the recorded events.
    pub fn take(&self) -> Vec<AppEvent> {
        // A poisoned lock only means a test thread panicked mid-push; the data is still usable.
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *events)
    }
}

impl EventSink for RecordingSink {
    fn emit(&self, event: AppEvent) {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::RemoteOp;

    #[test]
    fn recording_sink_records_and_clears() {
        let sink = RecordingSink::new();
        sink.emit(AppEvent::Warning("w".into()));
        sink.emit(AppEvent::RemotesUpdated {
            repo: "r1".into(),
            remotes: vec![],
        });
        let events = sink.take();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].repo().map(String::as_str), Some("r1"));
        assert_eq!(events[0].repo(), None);
        assert!(sink.take().is_empty());
    }

    fn every_variant() -> Vec<AppEvent> {
        let r = || RepoId::from("r1");
        vec![
            AppEvent::RepositoriesUpdated(vec![]),
            AppEvent::SelectedRepositoryUpdated(Some(r())),
            AppEvent::CommitsUpdated {
                repo: r(),
                commits: CommitWrapper::default(),
            },
            AppEvent::CommitsHeadUpdated { repo: r(), head: None },
            AppEvent::LocalBranchesUpdated {
                repo: r(),
                branches: vec![],
            },
            AppEvent::RemoteBranchesUpdated {
                repo: r(),
                branches: vec![],
            },
            AppEvent::RemotesUpdated {
                repo: r(),
                remotes: vec![],
            },
            AppEvent::GitFilesChanged {
                repo: r(),
                files: FileState::default(),
            },
            AppEvent::StashesUpdated {
                repo: r(),
                stashes: vec![],
            },
            AppEvent::WorktreesUpdated {
                repo: r(),
                worktrees: vec![],
            },
            AppEvent::RepoStatusUpdated {
                repo: r(),
                status: RepoStatus::default(),
            },
            AppEvent::CommitsSignatureUpdated(CommitSignature::new("a", "b")),
            AppEvent::AuthStatusUpdated {
                provider: Provider::Gitlab,
                connected: true,
            },
            AppEvent::Warning("w".into()),
            AppEvent::RemoteProgress {
                repo: Some(r()),
                progress: RemoteProgress {
                    op: RemoteOp::Fetch,
                    percent: Some(50),
                    message: "Receiving objects".into(),
                },
            },
        ]
    }

    #[test]
    fn names_are_unique_and_scope_is_consistent() {
        let events = every_variant();
        let mut names: Vec<&str> = events.iter().map(AppEvent::name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), events.len());
        let scoped = events.iter().filter(|e| e.repo().is_some()).count();
        assert_eq!(scoped, 10);
        for event in &events {
            assert_eq!(event.repo().is_some(), !event.is_for(Some("other")), "{}", event.name());
            assert!(event.is_for(Some("r1")));
            assert_eq!(event.is_for(None), event.repo().is_none());
        }
    }
}
