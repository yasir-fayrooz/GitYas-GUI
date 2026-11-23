//! merge (up-to-date / fast-forward / merge commit / conflicts), abort merge, pull.
//!
//! Decisions:
//! * No stash-around-merge. The old fast-forward path stashed, force-checked-out and popped; a conflicting pop
//!   left a half-applied state and the force checkout could clobber. We use a *safe* checkout instead: local
//!   changes that don't touch the merged files are carried over (like `git merge`); otherwise the merge is
//!   refused with the list of files, and nothing is modified.
//! * Merging into a branch that isn't checked out is only allowed as a fast-forward (moves the ref, no
//!   working-tree change). A real merge requires `target` to be HEAD.
//! * Conflicts are left in the index with the repository in the merge state (for WP6's conflict UI);
//!   [`abort_merge`] restores the pre-merge state without touching unrelated local changes.

use std::collections::BTreeSet;
use std::path::PathBuf;

use git2::{ErrorCode, Repository};
use gityas_core::models::CommitSignature;
use gityas_core::{GitYasError, Result};

use super::RemoteContext;
use super::transfer::{configured_upstream, fetch_remote};
use crate::branches::safe_checkout;
use crate::convert::oid_from_git;
use crate::git_error;
use crate::repository::{RepoState, ensure_clean_state, ensure_not_bare, fresh_index, git_signature, path_from_bytes, state};

pub use gityas_core::models::{AbortMergeOutcome, MergeOutcome, PullOutcome};

/// Resolves `name` (full ref name, or a short name git can DWIM) through symbolic refs (`origin/HEAD`, bug-log #2).
fn resolve_ref<'r>(repo: &'r Repository, name: &str) -> Result<git2::Reference<'r>> {
    let reference = match repo.find_reference(name) {
        Ok(r) => r,
        Err(e) if matches!(e.code(), ErrorCode::NotFound | ErrorCode::InvalidSpec) => repo
            .resolve_reference_from_short_name(name)
            .map_err(|_| GitYasError::NotFound(format!("{name:?} does not exist")))?,
        Err(e) => return Err(git_error(e)),
    };
    reference.resolve().map_err(|e| match e.code() {
        ErrorCode::NotFound => GitYasError::NotFound(format!("{name:?} points at a reference that does not exist")),
        _ => git_error(e),
    })
}

fn short_name(refname: &str) -> &str {
    ["refs/heads/", "refs/remotes/", "refs/tags/"]
        .iter()
        .find_map(|p| refname.strip_prefix(p))
        .unwrap_or(refname)
}

/// Merges `source` into the local branch `target` (both full ref names; `source` may be symbolic).
/// `signature` is needed only when a merge commit is created.
pub fn merge_branch(repo: &Repository, source: &str, target: &str, signature: Option<&CommitSignature>) -> Result<MergeOutcome> {
    ensure_not_bare(repo, "merge")?;
    ensure_clean_state(repo, "merge")?;
    if !target.starts_with("refs/heads/") {
        return Err(GitYasError::InvalidInput(format!(
            "merge target {target:?} must be a local branch (refs/heads/...)"
        )));
    }
    let source_ref = resolve_ref(repo, source)?;
    let source_name = source_ref.name().unwrap_or(source).to_owned();
    let source_commit = source_ref
        .peel_to_commit()
        .map_err(|e| GitYasError::InvalidInput(format!("{source:?} does not point at a commit: {}", e.message())))?;
    let annotated = repo.reference_to_annotated_commit(&source_ref).map_err(git_error)?;

    // Is `target` the checked-out branch (possibly unborn)?
    let head_is_target = match repo.find_reference("HEAD").map_err(git_error)?.symbolic_target_bytes() {
        Some(t) => t == target.as_bytes(),
        None => false,
    };
    let target_ref = match repo.find_reference(target) {
        Ok(r) => Some(r),
        Err(e) if e.code() == ErrorCode::NotFound => None,
        Err(e) => return Err(git_error(e)),
    };

    let Some(target_ref) = target_ref else {
        if !head_is_target {
            return Err(GitYasError::NotFound(format!("branch {:?} does not exist", short_name(target))));
        }
        // Unborn HEAD: the merge is a checkout of `source`.
        safe_checkout(repo, source_commit.as_object(), "merge")?;
        repo.reference(
            target,
            source_commit.id(),
            false,
            &format!("merge {}: initial", short_name(&source_name)),
        )
        .map_err(git_error)?;
        return Ok(MergeOutcome::FastForward {
            from: None,
            to: oid_from_git(source_commit.id())?,
        });
    };

    let target_oid = target_ref
        .target()
        .ok_or_else(|| GitYasError::InvalidInput(format!("{target:?} is a symbolic reference")))?;
    let (analysis, _) = repo.merge_analysis_for_ref(&target_ref, &[&annotated]).map_err(git_error)?;
    if analysis.is_up_to_date() {
        return Ok(MergeOutcome::UpToDate);
    }
    let reflog = format!("merge {}: Fast-forward", short_name(&source_name));
    if analysis.is_fast_forward() {
        if head_is_target {
            safe_checkout(repo, source_commit.as_object(), "merge")?;
        } else {
            ensure_not_checked_out_elsewhere(repo, target)?;
        }
        let mut target_ref = target_ref;
        target_ref.set_target(source_commit.id(), &reflog).map_err(git_error)?;
        return Ok(MergeOutcome::FastForward {
            from: Some(oid_from_git(target_oid)?),
            to: oid_from_git(source_commit.id())?,
        });
    }
    if !analysis.is_normal() {
        return Err(GitYasError::Git(format!(
            "cannot merge {source:?} into {target:?} (no common history?)"
        )));
    }
    if !head_is_target {
        return Err(GitYasError::InvalidInput(format!(
            "check out {:?} before merging into it (it cannot be fast-forwarded)",
            short_name(target)
        )));
    }
    let sig = git_signature(signature, "merge")?;
    ensure_index_matches_head(repo)?;

    let blocked = std::cell::RefCell::new(Vec::<PathBuf>::new());
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.safe().allow_conflicts(true).conflict_style_merge(true);
    checkout.notify_on(git2::CheckoutNotificationType::CONFLICT);
    checkout.notify(|_, path, _, _, _| {
        if let Some(p) = path {
            blocked.borrow_mut().push(p.to_path_buf());
        }
        true
    });
    let mut merge_opts = git2::MergeOptions::new();
    let merge_result = repo.merge(&[&annotated], Some(&mut merge_opts), Some(&mut checkout));
    drop(checkout);
    if let Err(e) = merge_result {
        let mut blocked = blocked.into_inner();
        if !blocked.is_empty() || matches!(e.code(), ErrorCode::Conflict | ErrorCode::MergeConflict) {
            if blocked.is_empty() {
                // Files the merge brings in = merge base -> source.
                let base = repo.merge_base(target_oid, source_commit.id()).map_err(git_error)?;
                let base_tree = commit_tree(repo, base)?;
                let source_tree = source_commit.tree().map_err(git_error)?;
                blocked = crate::branches::blocking_paths(repo, Some(&base_tree), &source_tree)?;
            }
            return Err(overwrite_error("merge", &blocked));
        }
        return Err(git_error(e));
    }

    let mut index = repo.index().map_err(git_error)?;
    if index.has_conflicts() {
        return Ok(MergeOutcome::Conflicts {
            paths: conflict_paths(&index)?,
        });
    }
    let tree_id = index.write_tree().map_err(git_error)?;
    let tree = repo.find_tree(tree_id).map_err(git_error)?;
    let head_commit = repo.find_commit(target_oid).map_err(git_error)?;
    let kind = if source_name.starts_with("refs/remotes/") {
        "remote-tracking branch"
    } else {
        "branch"
    };
    let message = format!("Merge {kind} '{}' into {}", short_name(&source_name), short_name(target));
    let commit = repo
        .commit(Some("HEAD"), &sig, &sig, &message, &tree, &[&head_commit, &source_commit])
        .map_err(git_error)?;
    repo.cleanup_state().map_err(git_error)?;
    Ok(MergeOutcome::Merged {
        commit: oid_from_git(commit)?,
    })
}

fn commit_tree(repo: &Repository, oid: git2::Oid) -> Result<git2::Tree<'_>> {
    repo.find_commit(oid).and_then(|c| c.tree()).map_err(git_error)
}

/// `action` is only logged: `GitYasError::Conflict` carries the paths and has its own user-facing message.
pub(crate) fn overwrite_error(action: &str, paths: &[PathBuf]) -> GitYasError {
    tracing::debug!(action, files = paths.len(), "refusing: local changes would be overwritten");
    GitYasError::Conflict(paths.to_vec())
}

/// Staged changes would be folded into the merge commit; refuse like git does.
fn ensure_index_matches_head(repo: &Repository) -> Result<()> {
    let head_tree = repo.head().and_then(|h| h.peel_to_tree()).map_err(git_error)?;
    let index = fresh_index(repo)?;
    if index.has_conflicts() {
        return Err(GitYasError::InvalidInput("resolve the existing conflicts first".to_owned()));
    }
    let diff = repo.diff_tree_to_index(Some(&head_tree), Some(&index), None).map_err(git_error)?;
    if diff.deltas().len() > 0 {
        return Err(GitYasError::InvalidInput(
            "you have staged changes; commit or stash them before merging".to_owned(),
        ));
    }
    Ok(())
}

fn ensure_not_checked_out_elsewhere(repo: &Repository, refname: &str) -> Result<()> {
    let worktrees = repo.worktrees().map_err(git_error)?;
    for name in worktrees.iter().flatten() {
        let wt = repo.find_worktree(name).map_err(git_error)?;
        // A pruned/missing worktree can't have anything checked out.
        let Ok(wt_repo) = Repository::open_from_worktree(&wt) else {
            continue;
        };
        let head = wt_repo.find_reference("HEAD").map_err(git_error)?;
        if head.symbolic_target_bytes() == Some(refname.as_bytes()) {
            return Err(GitYasError::InvalidInput(format!(
                "{:?} is checked out in worktree {}; merge there instead",
                short_name(refname),
                wt.path().display()
            )));
        }
    }
    Ok(())
}

pub(crate) fn conflict_paths(index: &git2::Index) -> Result<Vec<PathBuf>> {
    let mut paths = BTreeSet::new();
    for conflict in index.conflicts().map_err(git_error)? {
        let conflict = conflict.map_err(git_error)?;
        for entry in [conflict.our, conflict.their, conflict.ancestor].into_iter().flatten() {
            paths.insert(path_from_bytes(&entry.path)?);
        }
    }
    Ok(paths.into_iter().collect())
}

/// Aborts an in-progress merge (`git merge --abort` semantics): every path the merge touched (conflicted, or
/// staged by the merge) is reset to HEAD in the index and working tree; files the merge added are removed;
/// unrelated unstaged local changes are kept. Then the merge state (`MERGE_HEAD`, `MERGE_MSG`) is cleaned up.
///
/// The old command did `reset --hard HEAD`, destroying unrelated local changes, and ran in any state.
pub fn abort_merge(repo: &Repository) -> Result<AbortMergeOutcome> {
    ensure_not_bare(repo, "abort a merge")?;
    if state(repo) != RepoState::Merge {
        return Err(GitYasError::InvalidInput("no merge is in progress".to_owned()));
    }
    let head_commit = repo.head().and_then(|h| h.peel_to_commit()).map_err(git_error)?;
    let head_tree = head_commit.tree().map_err(git_error)?;
    let mut index = fresh_index(repo)?;

    let mut touched: BTreeSet<PathBuf> = conflict_paths(&index)?.into_iter().collect();
    let diff = repo.diff_tree_to_index(Some(&head_tree), Some(&index), None).map_err(git_error)?;
    for delta in diff.deltas() {
        for file in [delta.old_file(), delta.new_file()] {
            if let Some(p) = file.path() {
                touched.insert(p.to_path_buf());
            }
        }
    }

    let mut outcome = AbortMergeOutcome {
        restored: Vec::new(),
        removed: Vec::new(),
    };
    if !touched.is_empty() {
        // Drop the conflict stages first: for a conflicted path HEAD does not have (deleted by us, added by them),
        // `reset_default` would add an index entry with HEAD's mode, 0, and fail with "invalid entry mode".
        for path in conflict_paths(&index)? {
            index.conflict_remove(&path).map_err(git_error)?;
        }
        index.write().map_err(git_error)?;
        let paths: Vec<&PathBuf> = touched.iter().collect();
        repo.reset_default(Some(head_commit.as_object()), paths).map_err(git_error)?;
        index.read(true).map_err(git_error)?;
        let workdir = repo
            .workdir()
            .ok_or_else(|| GitYasError::InvalidInput("cannot abort a merge in a bare repository".to_owned()))?
            .to_path_buf();
        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout.force().disable_pathspec_match(true);
        let mut any_restore = false;
        for path in &touched {
            let in_head = match head_tree.get_path(path) {
                Ok(_) => true,
                Err(e) if e.code() == ErrorCode::NotFound => false,
                Err(e) => return Err(git_error(e)),
            };
            if in_head {
                checkout.path(path.as_path());
                any_restore = true;
                outcome.restored.push(path.clone());
            } else {
                let full = workdir.join(path);
                match std::fs::remove_file(&full) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(GitYasError::Io(format!("cannot remove {}: {e}", full.display()))),
                }
                outcome.removed.push(path.clone());
            }
        }
        if any_restore {
            repo.checkout_tree(head_tree.as_object(), Some(&mut checkout)).map_err(git_error)?;
        }
    }
    repo.cleanup_state().map_err(git_error)?;
    Ok(outcome)
}

/// Full ref name of `local_refname`'s upstream remote-tracking branch (`refs/remotes/origin/main`), if configured.
pub fn upstream_refname(repo: &Repository, local_refname: &str) -> Result<Option<String>> {
    match repo.branch_upstream_name(local_refname) {
        Ok(buf) => buf
            .as_str()
            .map(|s| Some(s.to_owned()))
            .ok_or_else(|| GitYasError::InvalidInput("upstream name is not valid UTF-8".to_owned())),
        Err(e) if e.code() == ErrorCode::NotFound => Ok(None),
        Err(e) => Err(git_error(e)),
    }
}

/// Fetches the upstream's remote (pruning deleted refs), then merges the upstream into `local_refname`.
pub fn pull(repo: &Repository, local_refname: &str, signature: Option<&CommitSignature>, ctx: &RemoteContext<'_>) -> Result<PullOutcome> {
    let (remote, _) = configured_upstream(repo, local_refname)?
        .ok_or_else(|| GitYasError::InvalidInput(format!("{:?} has no upstream branch", short_name(local_refname))))?;
    let upstream = upstream_refname(repo, local_refname)?
        .ok_or_else(|| GitYasError::InvalidInput(format!("{:?} has no upstream branch", short_name(local_refname))))?;
    let fetch = fetch_remote(repo, &remote, true, ctx)?;
    match repo.find_reference(&upstream) {
        Ok(_) => {}
        Err(e) if e.code() == ErrorCode::NotFound => {
            return Err(GitYasError::NotFound(format!(
                "the upstream branch {:?} no longer exists on {remote}",
                short_name(&upstream)
            )));
        }
        Err(e) => return Err(git_error(e)),
    }
    let merge = merge_branch(repo, &upstream, local_refname, signature)?;
    Ok(PullOutcome { fetch, merge })
}
