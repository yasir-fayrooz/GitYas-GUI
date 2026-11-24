//! Recompute parts of the active repository's state and emit the matching events. Shared by `open_repository`
//! (everything), the git command facade (what a command changed) and the watchers (what changed on disk).

use gityas_core::models::BranchKind;
use gityas_core::{AppEvent, GitYasError, Result};

use crate::session::SessionCtx;
use crate::{Engine, Inner, cache};

/// Which parts of the repository state to recompute and emit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Refresh {
    /// `CommitsUpdated` (history via the commit-graph cache, refs, tags).
    pub commits: bool,
    /// `CommitsHeadUpdated`.
    pub head: bool,
    /// `LocalBranchesUpdated` (also carries ahead/behind, so refresh after fetch too).
    pub local_branches: bool,
    /// `RemoteBranchesUpdated`.
    pub remote_branches: bool,
    /// `RemotesUpdated`.
    pub remotes: bool,
    /// `GitFilesChanged`.
    pub files: bool,
    /// `StashesUpdated`.
    pub stashes: bool,
    /// `RepoStatusUpdated` (operation in progress, shallow / bare).
    pub status: bool,
    /// `WorktreesUpdated` (linked worktrees and what each has checked out).
    pub worktrees: bool,
}

impl Refresh {
    pub const NONE: Refresh = Refresh {
        commits: false,
        head: false,
        local_branches: false,
        remote_branches: false,
        remotes: false,
        files: false,
        stashes: false,
        status: false,
        worktrees: false,
    };
    pub const ALL: Refresh = Refresh {
        commits: true,
        head: true,
        local_branches: true,
        remote_branches: true,
        remotes: true,
        files: true,
        stashes: true,
        status: true,
        worktrees: true,
    };
    /// Working tree / index changed (stage, unstage, discard, file watcher).
    pub const FILES: Refresh = Refresh {
        files: true,
        status: true,
        ..Refresh::NONE
    };
    /// HEAD moved (commit, checkout, merge, reset): history, head, branch ahead/behind and files.
    pub const HEAD: Refresh = Refresh {
        commits: true,
        head: true,
        local_branches: true,
        files: true,
        status: true,
        worktrees: true,
        ..Refresh::NONE
    };
    /// `refs/heads`, tags or `refs/stash` changed (branch create/delete, tag ops, stash).
    pub const LOCAL_REFS: Refresh = Refresh {
        commits: true,
        head: true,
        local_branches: true,
        stashes: true,
        worktrees: true,
        ..Refresh::NONE
    };
    /// Remote-tracking refs changed (fetch, push, remote branch delete).
    pub const REMOTE_REFS: Refresh = Refresh {
        commits: true,
        local_branches: true,
        remote_branches: true,
        remotes: true,
        ..Refresh::NONE
    };

    pub const fn union(self, other: Refresh) -> Refresh {
        Refresh {
            commits: self.commits || other.commits,
            head: self.head || other.head,
            local_branches: self.local_branches || other.local_branches,
            remote_branches: self.remote_branches || other.remote_branches,
            remotes: self.remotes || other.remotes,
            files: self.files || other.files,
            stashes: self.stashes || other.stashes,
            status: self.status || other.status,
            worktrees: self.worktrees || other.worktrees,
        }
    }

    pub const fn is_empty(self) -> bool {
        !(self.commits
            || self.head
            || self.local_branches
            || self.remote_branches
            || self.remotes
            || self.files
            || self.stashes
            || self.status
            || self.worktrees)
    }
}

/// Recomputes each requested part and emits its event. Parts are independent: one failing (e.g. an unreadable
/// status entry) does not stop the others from being emitted. Returns the first error, after emitting the rest.
pub(crate) fn refresh_blocking(inner: &Inner, ctx: &SessionCtx, repo: &git2::Repository, what: Refresh) -> Result<()> {
    let mut first_err: Option<GitYasError> = None;
    let mut part = |name: &str, result: Result<AppEvent>| match result {
        Ok(event) => inner.emit_scoped(ctx, event),
        Err(err) => {
            tracing::warn!(repo = %ctx.repo_id, part = name, "refresh failed: {err}");
            first_err.get_or_insert(err);
        }
    };
    let repo_id = || ctx.repo_id.clone();

    // A long history the engine has not loaded yet: its newest page first, the full history after the other parts,
    // so the graph, branches and working tree show while it loads.
    let preview = what.commits.then(|| cache::preview_commits(inner, ctx, repo)).flatten();
    let full_history_last = preview.is_some();
    if let Some(commits) = preview {
        part("commits preview", Ok(AppEvent::CommitsUpdated { repo: repo_id(), commits }));
    }
    let full_history = || cache::load_commits(inner, ctx, repo).map(|commits| AppEvent::CommitsUpdated { repo: repo_id(), commits });
    if what.commits && !full_history_last {
        part("commits", full_history());
    }
    if what.head {
        part(
            "head",
            gityas_git::commits::get_head(repo).map(|head| AppEvent::CommitsHeadUpdated {
                repo: repo_id(),
                head: Some(head),
            }),
        );
    }
    if what.local_branches {
        part(
            "local_branches",
            gityas_git::branches::list_branches(repo, Some(BranchKind::Local))
                .map(|branches| AppEvent::LocalBranchesUpdated { repo: repo_id(), branches }),
        );
    }
    if what.remote_branches {
        part(
            "remote_branches",
            gityas_git::branches::list_branches(repo, Some(BranchKind::Remote))
                .map(|branches| AppEvent::RemoteBranchesUpdated { repo: repo_id(), branches }),
        );
    }
    if what.remotes {
        part(
            "remotes",
            gityas_git::repository::remote_names(repo).map(|remotes| AppEvent::RemotesUpdated { repo: repo_id(), remotes }),
        );
    }
    if what.files {
        part(
            "files",
            gityas_git::status::get_status(repo).map(|files| AppEvent::GitFilesChanged { repo: repo_id(), files }),
        );
    }
    if what.stashes {
        part(
            "stashes",
            gityas_git::stash::list_stashes(repo).map(|stashes| AppEvent::StashesUpdated { repo: repo_id(), stashes }),
        );
    }
    if what.worktrees {
        part(
            "worktrees",
            gityas_git::worktrees::list_worktrees(repo).map(|worktrees| AppEvent::WorktreesUpdated {
                repo: repo_id(),
                worktrees,
            }),
        );
    }
    if what.status {
        part(
            "status",
            gityas_git::repository::status(repo).map(|status| AppEvent::RepoStatusUpdated { repo: repo_id(), status }),
        );
    }
    if full_history_last {
        part("commits", full_history());
    }
    first_err.map_or(Ok(()), Err)
}

impl Engine {
    /// Recomputes and re-emits the requested parts of the active repository's state (toolbar "Refresh").
    pub async fn refresh(&self, what: Refresh) -> Result<()> {
        if what.is_empty() {
            return Ok(());
        }
        self.with_repo(move |inner, ctx, repo| refresh_blocking(inner, ctx, repo, what))
            .await
    }
}
