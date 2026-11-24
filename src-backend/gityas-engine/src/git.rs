//! Git command facade. Thin async wrappers over `gityas-git` (no git2 logic of its own): run under
//! [`Engine::with_repo`], then [`crate::refresh::refresh_blocking`] what the command changed. Remote operations
//! register with the session's remote-op registry (cancel) and report `AppEvent::RemoteProgress`.
//!
//! Each command is `with_repo` → one `gityas-git` call → a refresh with the narrowest [`Refresh`] that covers what
//! the command changed. Read-only queries emit nothing. A refresh failure after a successful command is logged
//! (and the parts that worked are still emitted); the command's own result is returned.

use std::path::PathBuf;

use gityas_core::diff::{DiffTarget, FileDiff, HunkRef};
use gityas_core::models::{
    AbortMergeOutcome, Branch, BranchKind, CommitBody, CommitSignature, DeletedMergeAction, Delta, FetchSummary, FileState, Head,
    MergeConflict, MergeOutcome, PullOutcome, PushOutcome, RemoteOp, ResetMode, StashApplyOutcome, StashDetails, Worktree, WorktreeBranch,
};
use gityas_core::{GitYasError, Oid, Result};
use gityas_git::{branches, commits, diff, merge, refs, remote, repository, stash, worktree, worktrees};

use crate::refresh::{Refresh, refresh_blocking};
use crate::session::SessionCtx;
use crate::{CheckoutTarget, Engine, Inner};

#[cfg(test)]
mod tests;

use crate::progress::with_remote_ctx;

/// Tags created/deleted: only the history decorations (`CommitsUpdated` carries refs and tags).
const TAGS: Refresh = Refresh {
    commits: true,
    ..Refresh::NONE
};
/// `refs/heads/*` changed but HEAD did not move: decorations + local branch list.
const BRANCHES: Refresh = Refresh {
    commits: true,
    local_branches: true,
    worktrees: true,
    ..Refresh::NONE
};
/// `refs/stash` changed (it is listed in `CommitsUpdated.references` as `ReferenceKind::Stash`).
const STASH_REF: Refresh = Refresh {
    commits: true,
    stashes: true,
    ..Refresh::NONE
};

/// Fetch / pull: every ref list (local and remote branches, tags, stashes), whether or not the fetch moved a ref.
const ALL_REFS: Refresh = Refresh::REMOTE_REFS.union(Refresh::LOCAL_REFS);

/// Re-emits `what` after a mutation. `refresh_blocking` already logs each failing part; the command's own result
/// is what the caller returns, so the refresh error is only recorded here.
fn refresh_after(inner: &Inner, ctx: &SessionCtx, repo: &git2::Repository, what: Refresh) {
    if what.is_empty() {
        return;
    }
    if let Err(err) = refresh_blocking(inner, ctx, repo, what) {
        tracing::debug!(repo = %ctx.repo_id, "refresh after command incomplete: {err}");
    }
}

/// The stored commit signature, or the user-facing "set your signature" error.
fn required_signature(inner: &Inner) -> Result<CommitSignature> {
    let settings = inner.storage.user().settings()?;
    settings.require_signature().cloned()
}

/// The stored signature if any (merge/pull only need it when a merge commit is created; `gityas-git` reports
/// a clear error in that case).
fn optional_signature(inner: &Inner) -> Result<Option<CommitSignature>> {
    Ok(inner.storage.user().settings()?.commit_signature)
}

/// What a merge outcome changed. Conflicts leave the merge state, conflicted index and worktree: HEAD did not
/// move, but files and the repository state did, so they get the same refresh as a moved HEAD.
fn merge_refresh(outcome: &MergeOutcome) -> Refresh {
    match outcome {
        MergeOutcome::UpToDate => Refresh::NONE,
        MergeOutcome::FastForward { .. } | MergeOutcome::Merged { .. } | MergeOutcome::Conflicts { .. } => Refresh::HEAD,
    }
}

impl Engine {
    pub async fn head(&self) -> Result<Option<Head>> {
        self.with_repo(|_, _, repo| commits::get_head(repo).map(Some)).await
    }

    /// Full commit message, fetched on demand (flags non-UTF-8 messages).
    pub async fn commit_body(&self, commit: Oid) -> Result<CommitBody> {
        self.with_repo(move |_, _, repo| commits::commit_body(repo, &commit)).await
    }

    /// Commits the index; works on an unborn HEAD. Fails if no valid commit signature is set.
    pub async fn commit_staged_files(&self, message: String, description: Option<String>) -> Result<Oid> {
        self.with_repo(move |inner, ctx, repo| {
            let signature = required_signature(inner)?;
            let oid = commits::commit_staged_files(repo, &signature, &message, description.as_deref())?;
            refresh_after(inner, ctx, repo, Refresh::HEAD);
            Ok(oid)
        })
        .await
    }

    /// Replaces the message of HEAD or one of its ancestors, rewriting the commits above it; returns the reworded
    /// commit's new id. No signature is needed (authors and committers are kept).
    pub async fn reword_commit(&self, commit: Oid, message: String) -> Result<Oid> {
        self.with_repo(move |inner, ctx, repo| {
            let oid = commits::reword_commit(repo, &commit, &message)?;
            refresh_after(inner, ctx, repo, Refresh::HEAD);
            Ok(oid)
        })
        .await
    }

    /// Moves the checked-out branch (or detached HEAD) to `commit`; see [`ResetMode`] for what happens to changes.
    pub async fn reset_to_commit(&self, commit: Oid, mode: ResetMode) -> Result<()> {
        self.with_repo(move |inner, ctx, repo| {
            commits::reset_to_commit(repo, &commit, mode)?;
            refresh_after(inner, ctx, repo, Refresh::HEAD);
            Ok(())
        })
        .await
    }

    /// Lightweight tag when `message` is `None`, annotated otherwise (annotated tags need the commit signature).
    pub async fn create_tag(&self, commit: Oid, name: String, message: Option<String>) -> Result<()> {
        self.with_repo(move |inner, ctx, repo| {
            let signature = match message {
                Some(_) => Some(required_signature(inner)?),
                None => None,
            };
            refs::create_tag(repo, &name, &commit, message.as_deref(), signature.as_ref())?;
            refresh_after(inner, ctx, repo, TAGS);
            Ok(())
        })
        .await
    }

    pub async fn delete_tag(&self, name: String) -> Result<()> {
        self.with_repo(move |inner, ctx, repo| {
            refs::delete_tag(repo, &name)?;
            refresh_after(inner, ctx, repo, TAGS);
            Ok(())
        })
        .await
    }

    /// Pushes a local tag to `remote`. No local ref changes, so nothing is refreshed (call
    /// [`Engine::remote_tag_names`] to update a "pushed" indicator).
    pub async fn push_tag(&self, remote: String, name: String) -> Result<()> {
        self.with_repo(move |inner, ctx, repo| {
            with_remote_ctx(inner, ctx, RemoteOp::Push, |rctx| refs::push_tag(repo, &remote, &name, rctx))
        })
        .await
    }

    /// Deletes a tag on `remote` (the local tag is kept). No local ref changes, nothing is refreshed.
    pub async fn delete_remote_tag(&self, remote: String, name: String) -> Result<()> {
        self.with_repo(move |inner, ctx, repo| {
            with_remote_ctx(inner, ctx, RemoteOp::Push, |rctx| {
                refs::delete_remote_tag(repo, &remote, &name, rctx)
            })
        })
        .await
    }

    /// Tag names advertised by `remote` (network round trip; read-only, emits only progress). Not part of
    /// `CommitsUpdated` because it needs the network: the UI calls this on demand (e.g. when opening a tag's context
    /// menu or after `push_tag`).
    pub async fn remote_tag_names(&self, remote: String) -> Result<Vec<String>> {
        self.with_repo(move |inner, ctx, repo| {
            with_remote_ctx(inner, ctx, RemoteOp::Fetch, |rctx| refs::list_remote_tag_names(repo, &remote, rctx))
        })
        .await
    }

    // ---- Branches & remotes ----------------------------------------------------------------------------------------

    pub async fn branches(&self, kind: BranchKind) -> Result<Vec<Branch>> {
        self.with_repo(move |_, _, repo| branches::list_branches(repo, Some(kind))).await
    }

    pub async fn remotes(&self) -> Result<Vec<String>> {
        self.with_repo(|_, _, repo| repository::remote_names(repo)).await
    }

    pub async fn checkout(&self, target: CheckoutTarget) -> Result<()> {
        self.with_repo(move |inner, ctx, repo| {
            match &target {
                CheckoutTarget::Branch(refname) => branches::checkout_branch(repo, refname).map(drop)?,
                CheckoutTarget::Commit(oid) => branches::checkout_commit(repo, oid)?,
            }
            refresh_after(inner, ctx, repo, Refresh::HEAD);
            Ok(())
        })
        .await
    }

    /// Creates a local branch at `start` (HEAD when `None`), checking it out when `checkout`. Returns its full ref name.
    pub async fn create_branch(&self, name: String, start: Option<Oid>, checkout: bool) -> Result<String> {
        self.with_repo(move |inner, ctx, repo| {
            let refname = branches::create_branch(repo, &name, start.as_ref(), checkout)?;
            refresh_after(inner, ctx, repo, if checkout { Refresh::HEAD } else { BRANCHES });
            Ok(refname)
        })
        .await
    }

    /// Pushes the current branch as `remote/name` and sets it as upstream.
    pub async fn create_remote_branch(&self, remote: String, name: String) -> Result<()> {
        self.publish(None, remote, name).await.map(drop)
    }

    /// Pushes local branch `refname` (`refs/heads/x`, need not be checked out) as `remote/name` and sets it as its
    /// upstream (push dialog for a branch without upstream when there are several remotes). Returns the upstream's
    /// short name (`origin/x`). Fails if `remote/name` is already known locally (fetch first).
    pub async fn publish_branch(&self, refname: String, remote: String, name: String) -> Result<String> {
        self.publish(Some(refname), remote, name).await
    }

    async fn publish(&self, refname: Option<String>, remote: String, name: String) -> Result<String> {
        self.with_repo(move |inner, ctx, repo| {
            let local = match refname {
                Some(refname) => refname,
                None => match commits::get_head(repo)? {
                    Head::Branch { refname, .. } => refname,
                    Head::Unborn { name, .. } => {
                        return Err(GitYasError::InvalidInput(format!(
                            "branch {name:?} has no commits yet; commit before publishing it"
                        )));
                    }
                    Head::Detached { .. } => {
                        return Err(GitYasError::InvalidInput(
                            "HEAD is detached; check out a branch to publish it".into(),
                        ));
                    }
                },
            };
            let result = with_remote_ctx(inner, ctx, RemoteOp::Push, |rctx| {
                branches::create_remote_branch(repo, &local, &remote, Some(&name), rctx)
            });
            refresh_after(inner, ctx, repo, if result.is_ok() { Refresh::REMOTE_REFS } else { Refresh::NONE });
            result
        })
        .await
    }

    /// Renames local branch `refname` (`refs/heads/x`) to `new_name`; returns the new full ref name.
    pub async fn rename_branch(&self, refname: String, new_name: String) -> Result<String> {
        self.with_repo(move |inner, ctx, repo| {
            let was_head = repo.head().ok().is_some_and(|h| h.name() == Some(refname.as_str()));
            let renamed = branches::rename_local_branch(repo, &refname, &new_name)?;
            refresh_after(inner, ctx, repo, if was_head { Refresh::HEAD } else { BRANCHES });
            Ok(renamed)
        })
        .await
    }

    /// `refname` is the full ref name; remote branches are deleted on the remote. Without `force`, a local branch
    /// that isn't merged into its upstream (or HEAD) is refused.
    pub async fn delete_branch(&self, refname: String, force: bool) -> Result<()> {
        self.with_repo(move |inner, ctx, repo| {
            if refname.starts_with("refs/heads/") {
                branches::delete_local_branch(repo, &refname, force)?;
                refresh_after(inner, ctx, repo, BRANCHES);
            } else if refname.starts_with("refs/remotes/") {
                with_remote_ctx(inner, ctx, RemoteOp::Push, |rctx| {
                    branches::delete_remote_branch(repo, &refname, rctx)
                })?;
                refresh_after(inner, ctx, repo, Refresh::REMOTE_REFS);
            } else {
                return Err(GitYasError::InvalidInput(format!(
                    "{refname:?} is not a branch (expected refs/heads/… or refs/remotes/…)"
                )));
            }
            Ok(())
        })
        .await
    }

    // ---- Worktrees -------------------------------------------------------------------------------------------------

    /// Every worktree of the repository, the main one first (also emitted as `WorktreesUpdated` on refresh).
    pub async fn worktrees(&self) -> Result<Vec<Worktree>> {
        self.with_repo(|_, _, repo| worktrees::list_worktrees(repo)).await
    }

    /// Adds a linked worktree at `path` (missing or empty folder) checking out `branch` (a new branch is created
    /// first). The new worktree is not opened; register it with `add_repository` to open it.
    pub async fn add_worktree(&self, path: PathBuf, branch: WorktreeBranch) -> Result<Worktree> {
        self.with_repo(move |inner, ctx, repo| {
            let added = worktrees::add_worktree(repo, &path, &branch)?;
            refresh_after(inner, ctx, repo, BRANCHES);
            Ok(added)
        })
        .await
    }

    /// Removes linked worktree `name` (its folder and admin data; its branch is kept). Without `force`, a worktree
    /// with changes is refused with `WorktreeDirty` and a locked one with `InvalidInput`.
    pub async fn remove_worktree(&self, name: String, force: bool) -> Result<()> {
        self.with_repo(move |inner, ctx, repo| {
            worktrees::remove_worktree(repo, &name, force)?;
            refresh_after(inner, ctx, repo, BRANCHES);
            Ok(())
        })
        .await
    }

    // ---- Remote operations -----------------------------------------------------------------------------------------

    /// Fetches `remote`, or all remotes when `None`, pruning remote-tracking refs deleted on the remote. A
    /// repository without remotes is a no-op, not an error. Then refreshes every ref list (branches, tags, stashes),
    /// so changes made outside GitYas show up too.
    pub async fn fetch(&self, remote: Option<String>) -> Result<FetchSummary> {
        self.with_repo(move |inner, ctx, repo| {
            let summary = with_remote_ctx(inner, ctx, RemoteOp::Fetch, |rctx| {
                remote::fetch(repo, remote.as_deref(), true, rctx)
            })?;
            if !(summary.fetched.is_empty() && summary.failed.is_empty()) {
                refresh_after(inner, ctx, repo, ALL_REFS);
            }
            Ok(summary)
        })
        .await
    }

    /// Fetch (pruning) + merge of the branch's upstream into the local branch (`refname` = full local ref name).
    /// Refreshes every ref list, like [`Engine::fetch`].
    pub async fn pull(&self, refname: String) -> Result<PullOutcome> {
        self.with_repo(move |inner, ctx, repo| {
            let signature = optional_signature(inner)?;
            let result = with_remote_ctx(inner, ctx, RemoteOp::Pull, |rctx| {
                remote::pull(repo, &refname, signature.as_ref(), rctx)
            });
            let what = match &result {
                Ok(outcome) => ALL_REFS.union(merge_refresh(&outcome.merge)),
                // The fetch half may have updated remote-tracking refs before the merge was refused.
                Err(GitYasError::Cancelled | GitYasError::NoActiveRepository) => Refresh::NONE,
                Err(_) => ALL_REFS,
            };
            refresh_after(inner, ctx, repo, what);
            result
        })
        .await
    }

    /// Pushes the branch to its upstream (setting one if missing). Does not push tags (see [`Engine::push_tag`]).
    pub async fn push(&self, refname: String, force: bool) -> Result<PushOutcome> {
        self.with_repo(move |inner, ctx, repo| {
            let outcome = with_remote_ctx(inner, ctx, RemoteOp::Push, |rctx| remote::push_branch(repo, &refname, force, rctx))?;
            refresh_after(inner, ctx, repo, Refresh::REMOTE_REFS);
            Ok(outcome)
        })
        .await
    }

    /// Merges `source` into `target` (full ref names); fast-forwards when possible.
    pub async fn merge(&self, source: String, target: String) -> Result<MergeOutcome> {
        self.with_repo(move |inner, ctx, repo| {
            let signature = optional_signature(inner)?;
            let outcome = remote::merge_branch(repo, &source, &target, signature.as_ref())?;
            refresh_after(inner, ctx, repo, merge_refresh(&outcome));
            Ok(outcome)
        })
        .await
    }

    /// Aborts in-flight remote operations (status-bar Cancel); they fail with `GitYasError::Cancelled`. Returns
    /// how many were signalled. Does not take the command lock.
    pub fn cancel_remote_operation(&self) -> usize {
        self.inner.sessions.cancel_remote_ops()
    }

    pub async fn abort_merge(&self) -> Result<AbortMergeOutcome> {
        self.with_repo(|inner, ctx, repo| {
            let outcome = remote::abort_merge(repo)?;
            refresh_after(inner, ctx, repo, Refresh::FILES);
            Ok(outcome)
        })
        .await
    }

    // ---- Working tree ----------------------------------------------------------------------------------------------

    pub async fn file_state(&self) -> Result<FileState> {
        self.with_repo(|inner, ctx, repo| crate::worktree_status::load(inner, ctx, repo, None))
            .await
    }

    pub async fn stage_files(&self, files: Vec<Delta>) -> Result<()> {
        self.files_op(move |repo| worktree::stage_files(repo, &files)).await
    }

    pub async fn unstage_files(&self, files: Vec<Delta>) -> Result<()> {
        self.files_op(move |repo| worktree::unstage_files(repo, &files)).await
    }

    /// Discards working-tree changes only (staged content is kept; unstage first for "discard all").
    pub async fn discard_files(&self, files: Vec<Delta>) -> Result<()> {
        self.files_op(move |repo| worktree::discard_files(repo, &files)).await
    }

    /// Fails with `GitYasError::StaleHunk` if the hunk's content hash no longer matches.
    pub async fn stage_hunk(&self, file: Delta, hunk: HunkRef) -> Result<()> {
        self.files_op(move |repo| worktree::stage_hunk(repo, &file, &hunk)).await
    }

    pub async fn unstage_hunk(&self, file: Delta, hunk: HunkRef) -> Result<()> {
        self.files_op(move |repo| worktree::unstage_hunk(repo, &file, &hunk)).await
    }

    pub async fn discard_hunk(&self, file: Delta, hunk: HunkRef) -> Result<()> {
        self.files_op(move |repo| worktree::discard_hunk(repo, &file, &hunk)).await
    }

    /// A working-tree/index mutation followed by a `GitFilesChanged` refresh.
    async fn files_op<F>(&self, op: F) -> Result<()>
    where
        F: FnOnce(&git2::Repository) -> Result<()> + Send + 'static,
    {
        self.with_repo(move |inner, ctx, repo| {
            op(repo)?;
            refresh_after(inner, ctx, repo, Refresh::FILES);
            Ok(())
        })
        .await
    }

    // ---- Stash -----------------------------------------------------------------------------------------------------

    /// Stashes only `files`; untracked ones among them only when `include_untracked`.
    pub async fn stash_files(&self, files: Vec<Delta>, message: Option<String>, include_untracked: bool) -> Result<()> {
        self.with_repo(move |inner, ctx, repo| {
            let signature = required_signature(inner)?;
            worktree::stash_files(repo, &signature, &files, message.as_deref(), include_untracked)?;
            refresh_after(inner, ctx, repo, Refresh::FILES.union(STASH_REF));
            Ok(())
        })
        .await
    }

    /// Stashes all changes (toolbar Stash); untracked files only when `include_untracked`. Nothing to stash is an
    /// `InvalidInput` error so the UI can say so (git itself prints "No local changes to save").
    pub async fn stash_all(&self, message: Option<String>, include_untracked: bool) -> Result<()> {
        self.with_repo(move |inner, ctx, repo| {
            let signature = required_signature(inner)?;
            match worktree::create_stash(repo, &signature, message.as_deref(), include_untracked)? {
                Some(_) => {
                    refresh_after(inner, ctx, repo, Refresh::FILES.union(STASH_REF));
                    Ok(())
                }
                None => Err(GitYasError::InvalidInput("there are no local changes to stash".into())),
            }
        })
        .await
    }

    pub async fn stashes(&self) -> Result<Vec<StashDetails>> {
        self.with_repo(|_, _, repo| stash::list_stashes(repo)).await
    }

    /// Applies `stash@{index}` (the stash is kept). Conflicts are an outcome, not an error.
    pub async fn apply_stash(&self, index: usize) -> Result<StashApplyOutcome> {
        self.with_repo(move |inner, ctx, repo| {
            let outcome = stash::apply_stash(repo, index, false)?;
            refresh_after(inner, ctx, repo, Refresh::FILES);
            Ok(outcome)
        })
        .await
    }

    /// Applies and drops the stash; on conflicts the stash is kept.
    pub async fn pop_stash(&self, index: usize) -> Result<StashApplyOutcome> {
        self.with_repo(move |inner, ctx, repo| {
            let outcome = stash::pop_stash(repo, index)?;
            let what = match outcome {
                StashApplyOutcome::Applied => Refresh::FILES.union(STASH_REF),
                StashApplyOutcome::Conflicts { .. } => Refresh::FILES,
            };
            refresh_after(inner, ctx, repo, what);
            Ok(outcome)
        })
        .await
    }

    pub async fn delete_stash(&self, index: usize) -> Result<()> {
        self.with_repo(move |inner, ctx, repo| {
            stash::drop_stash(repo, index)?;
            refresh_after(inner, ctx, repo, STASH_REF);
            Ok(())
        })
        .await
    }

    // ---- Diff & conflicts ------------------------------------------------------------------------------------------

    /// Structured diff with the engine's size cap (`EngineOptions::diff`).
    pub async fn file_diff(&self, target: DiffTarget, file: Delta) -> Result<FileDiff> {
        self.with_repo(move |inner, _, repo| diff::get_file_diff(repo, &target, &file, &inner.options.diff))
            .await
    }

    /// Like [`Engine::file_diff`] with a size cap of `max_bytes` per side instead of the engine's (the diff
    /// viewer's "Show anyway" for `DiffKind::TooLarge`). Other `DiffConfig` fields keep the engine's values.
    pub async fn file_diff_with_limit(&self, target: DiffTarget, file: Delta, max_bytes: u64) -> Result<FileDiff> {
        self.with_repo(move |inner, _, repo| {
            let mut config = inner.options.diff.clone();
            config.max_bytes = max_bytes;
            diff::get_file_diff(repo, &target, &file, &config)
        })
        .await
    }

    /// Conflict contents for the merge editor; when `sides.is_text()` is false (binary, symlink, …) or a side was
    /// deleted, the UI offers [`Engine::resolve_conflict_with_action`] instead of the text editor.
    pub async fn merge_conflict(&self, path: PathBuf) -> Result<MergeConflict> {
        self.with_repo(move |_, _, repo| merge::get_merge_conflict(repo, &path)).await
    }

    /// Writes the resolved content and stages the path. The merge stays in progress (the next commit records
    /// `MERGE_HEAD`).
    pub async fn resolve_conflict(&self, path: PathBuf, resolved_content: String) -> Result<()> {
        self.files_op(move |repo| merge::resolve_conflict(repo, &path, &resolved_content))
            .await
    }

    /// "Mark resolved": stages the conflicted `paths` as they are in the working tree (`git add`).
    pub async fn mark_resolved(&self, paths: Vec<PathBuf>) -> Result<()> {
        self.files_op(move |repo| merge::mark_resolved(repo, &paths)).await
    }

    /// Takes one version as-is (any content: binary, symlink, submodule) or deletes the file, and stages it.
    pub async fn resolve_conflict_with_action(&self, path: PathBuf, action: DeletedMergeAction) -> Result<()> {
        self.files_op(move |repo| merge::resolve_conflict_with_action(repo, &path, action))
            .await
    }
}
