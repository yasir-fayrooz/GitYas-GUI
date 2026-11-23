//! Stash list / details / apply / pop / drop. (Path-limited stash creation is in `worktree`.)
//!
//! Stashes are read from the `refs/stash` reflog rather than `git2::Repository::stash_foreach`, which `unwrap`s
//! the message as UTF-8 and would panic on a non-UTF-8 stash message.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::PathBuf;

use git2::{ErrorCode, Repository};
use gityas_core::models::{ChangeCounts, Delta, DeltaChange, Stash, StashDetails};
use gityas_core::{GitYasError, Result};

use crate::convert::oid_from_git;
use crate::git_error;
use crate::remote::merge_flow_overwrite_error;
use crate::repository::{ensure_not_bare, fresh_index, signature_from_git};

pub use gityas_core::models::StashApplyOutcome;

fn stash_entries(repo: &Repository) -> Result<Vec<(git2::Oid, String)>> {
    let reflog = match repo.reflog("refs/stash") {
        Ok(r) => r,
        Err(e) if e.code() == ErrorCode::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(git_error(e)),
    };
    Ok(reflog
        .iter()
        .map(|entry| {
            // Display text; a non-UTF-8 message is decoded lossily instead of panicking.
            let message = entry
                .message_bytes()
                .map(|m| String::from_utf8_lossy(m).into_owned())
                .unwrap_or_default();
            (entry.id_new(), message)
        })
        .collect())
}

fn stash_oid(repo: &Repository, index: usize) -> Result<git2::Oid> {
    stash_entries(repo)?
        .get(index)
        .map(|(oid, _)| *oid)
        .ok_or_else(|| GitYasError::NotFound(format!("stash@{{{index}}} does not exist")))
}

fn change_of(status: git2::Delta) -> Option<DeltaChange> {
    Some(match status {
        git2::Delta::Unmodified => return None,
        git2::Delta::Added => DeltaChange::Added,
        git2::Delta::Deleted => DeltaChange::Deleted,
        git2::Delta::Modified => DeltaChange::Modified,
        git2::Delta::Renamed => DeltaChange::Renamed,
        git2::Delta::Copied => DeltaChange::Copied,
        git2::Delta::Ignored => DeltaChange::Ignored,
        git2::Delta::Untracked => DeltaChange::Untracked,
        git2::Delta::Typechange => DeltaChange::Typechange,
        git2::Delta::Unreadable => DeltaChange::Unreadable,
        git2::Delta::Conflicted => DeltaChange::Conflicted,
    })
}

fn details(repo: &Repository, index: usize, oid: git2::Oid, message: String) -> Result<StashDetails> {
    let commit = repo.find_commit(oid).map_err(git_error)?;
    let tree = commit.tree().map_err(git_error)?;
    // parent 0: the commit stashed on; parent 2 (optional): untracked files.
    let base_tree = commit.parent(0).and_then(|p| p.tree()).map_err(git_error)?;
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    let mut collect = |diff: git2::Diff<'_>, untracked: bool| -> Result<()> {
        for delta in diff.deltas() {
            let Some(change) = change_of(delta.status()) else { continue };
            let change = if untracked && change == DeltaChange::Added {
                DeltaChange::Untracked
            } else {
                change
            };
            let new_path = delta.new_file().path().map(PathBuf::from);
            let old_path = delta.old_file().path().map(PathBuf::from);
            let Some(path) = new_path.clone().or_else(|| old_path.clone()) else {
                continue;
            };
            if !seen.insert(path.clone()) {
                continue;
            }
            let old_path = match change {
                DeltaChange::Renamed | DeltaChange::Copied => old_path.filter(|o| *o != path),
                _ => None,
            };
            files.push(Delta { path, old_path, change });
        }
        Ok(())
    };
    collect(
        repo.diff_tree_to_tree(Some(&base_tree), Some(&tree), None).map_err(git_error)?,
        false,
    )?;
    if commit.parent_count() > 2 {
        let untracked = commit.parent(2).and_then(|p| p.tree()).map_err(git_error)?;
        collect(repo.diff_tree_to_tree(None, Some(&untracked), None).map_err(git_error)?, true)?;
    }
    let author = commit.author();
    Ok(StashDetails {
        stash: Stash {
            index,
            id: oid_from_git(oid)?,
            message,
            author: signature_from_git(&author),
            time: commit.time().seconds(),
            counts: ChangeCounts::from_deltas(&files),
        },
        files,
    })
}

/// All stashes, newest first (`stash@{0}`), with file lists and added/modified/deleted counts.
pub fn list_stashes(repo: &Repository) -> Result<Vec<StashDetails>> {
    stash_entries(repo)?
        .into_iter()
        .enumerate()
        .map(|(i, (oid, msg))| details(repo, i, oid, msg))
        .collect()
}

pub fn stash_details(repo: &Repository, index: usize) -> Result<StashDetails> {
    let (oid, msg) = stash_entries(repo)?
        .into_iter()
        .nth(index)
        .ok_or_else(|| GitYasError::NotFound(format!("stash@{{{index}}} does not exist")))?;
    details(repo, index, oid, msg)
}

/// Applies `stash@{index}` (tracked changes, plus untracked files if stashed). `reinstate_index` also restores
/// what was staged. Refuses (nothing modified) when files with local changes would be overwritten or when the
/// index has staged changes (libgit2 requirement).
pub fn apply_stash(repo: &mut Repository, index: usize, reinstate_index: bool) -> Result<StashApplyOutcome> {
    ensure_not_bare(repo, "apply a stash")?;
    stash_oid(repo, index)?;
    if fresh_index(repo)?.has_conflicts() {
        return Err(GitYasError::InvalidInput("resolve the conflicts in the index first".to_owned()));
    }
    let blocked = RefCell::new(Vec::<PathBuf>::new());
    let result = {
        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout.notify_on(git2::CheckoutNotificationType::CONFLICT);
        checkout.notify(|_, path, _, _, _| {
            if let Some(p) = path {
                blocked.borrow_mut().push(p.to_path_buf());
            }
            true
        });
        let mut opts = git2::StashApplyOptions::new();
        opts.checkout_options(checkout);
        if reinstate_index {
            opts.reinstantiate_index();
        }
        repo.stash_apply(index, Some(&mut opts))
    };
    if let Err(e) = result {
        let blocked = blocked.into_inner();
        return Err(match e.code() {
            ErrorCode::Uncommitted => {
                GitYasError::InvalidInput("you have staged changes; commit or unstage them before applying a stash".to_owned())
            }
            ErrorCode::Conflict if reinstate_index && blocked.is_empty() => GitYasError::InvalidInput(
                "the stashed index conflicts with the current state; apply without restoring the index".to_owned(),
            ),
            ErrorCode::Conflict | ErrorCode::MergeConflict => merge_flow_overwrite_error("applying the stash", &blocked),
            _ => git_error(e),
        });
    }
    let index_after = repo.index().map_err(git_error)?;
    if index_after.has_conflicts() {
        return Ok(StashApplyOutcome::Conflicts {
            paths: crate::remote::conflict_paths_of(&index_after)?,
        });
    }
    Ok(StashApplyOutcome::Applied)
}

/// Applies `stash@{index}` and drops it only if it applied cleanly (`git stash pop` semantics).
pub fn pop_stash(repo: &mut Repository, index: usize) -> Result<StashApplyOutcome> {
    let oid = stash_oid(repo, index)?;
    let outcome = apply_stash(repo, index, false)?;
    if outcome == StashApplyOutcome::Applied {
        // Guard against the list having shifted (another process) between apply and drop.
        if stash_oid(repo, index)? != oid {
            return Err(GitYasError::Other(format!(
                "the stash list changed while applying; stash@{{{index}}} was applied but not dropped"
            )));
        }
        repo.stash_drop(index).map_err(git_error)?;
    }
    Ok(outcome)
}

pub fn drop_stash(repo: &mut Repository, index: usize) -> Result<()> {
    stash_oid(repo, index)?;
    repo.stash_drop(index).map_err(git_error)
}
