//! Stage / unstage / discard (file and hunk, via libgit2 `apply` with a `HunkRef` content-hash check) and
//! path-limited stash. No `git` CLI.
//!
//! File operations act on the *current* state of each path, not on the (possibly stale) `DeltaChange` the UI sent:
//! * [`stage_files`]: path exists in the working tree ⇒ add it (content, mode, submodule gitlink); missing ⇒ remove
//!   it from the index. Renames stage both the new and the old path. (`git add -A -- <paths>`.)
//! * [`unstage_files`]: each path's index entry is reset to HEAD's (removed if HEAD has none, which covers the
//!   unborn-HEAD case). Only the given paths are touched (the old unborn path unstaged *everything*). Literal paths,
//!   never globs. (`git reset -- <paths>`.)
//! * [`discard_files`]: discards **working-tree** changes relative to the index (`git restore -- <paths>`); a path
//!   not in the index (untracked) is deleted (`git clean`), directories are never deleted. Staged changes are kept;
//!   the old code also threw away staged content, which is more destructive than the UI's "discard" suggests.
//! * Conflicted paths are refused: conflicts are resolved through the merge-conflict API.
//!
//! Hunk operations live in [`hunks`], stash creation in [`stash`].

use std::path::{Component, Path, PathBuf};

use git2::Repository;
use git2::build::CheckoutBuilder;
use gityas_core::models::{Delta, DeltaChange};
use gityas_core::{GitYasError, Result};

use crate::commits::head_commit;
use crate::git_error;
use crate::status::ensure_workdir;

mod hunks;
mod stash;

pub use hunks::{discard_hunk, hunk_diff_options, stage_hunk, unstage_hunk};
pub use stash::{create_stash, stash_files};

/// Rejects paths that could escape the working tree or confuse libgit2: absolute, `..`, `.`, empty, or NUL bytes.
pub(crate) fn validate_rel_path(path: &Path) -> Result<()> {
    let invalid = || GitYasError::InvalidInput(format!("not a repository-relative file path: {}", path.display()));
    if path.as_os_str().is_empty() || path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(invalid());
    }
    if path.components().all(|c| matches!(c, Component::Normal(_))) {
        Ok(())
    } else {
        Err(invalid())
    }
}

/// The delta's path plus its old path for renames/copies, validated.
pub(crate) fn delta_paths(delta: &Delta) -> Result<Vec<&Path>> {
    let mut paths = vec![delta.path.as_path()];
    if let Some(old) = delta.old_path.as_deref()
        && old != delta.path
    {
        paths.push(old);
    }
    for path in &paths {
        validate_rel_path(path)?;
    }
    Ok(paths)
}

/// Does anything (file, symlink, directory) exist at `path`? Symlinks are not followed.
pub(crate) fn exists_no_follow(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn has_conflict(index: &git2::Index, path: &Path) -> bool {
    (1..=3).any(|stage| index.get_path(path, stage).is_some())
}

fn refuse_conflicts(index: &git2::Index, delta: &Delta, paths: &[&Path]) -> Result<()> {
    if delta.change == DeltaChange::Conflicted || paths.iter().any(|p| has_conflict(index, p)) {
        return Err(GitYasError::InvalidInput(format!(
            "{} has merge conflicts; resolve them in the conflict editor",
            delta.path.display()
        )));
    }
    Ok(())
}

/// Removes an untracked file or symlink from the working tree. Directories are refused.
pub(crate) fn remove_worktree_file(full: &Path) -> Result<()> {
    match std::fs::symlink_metadata(full) {
        Ok(meta) if meta.is_dir() => Err(GitYasError::InvalidInput(format!(
            "refusing to delete directory {}",
            full.display()
        ))),
        Ok(_) => Ok(std::fs::remove_file(full)?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Stages the given files (see the module docs).
pub fn stage_files(repo: &Repository, files: &[Delta]) -> Result<()> {
    let workdir = ensure_workdir(repo)?;
    let mut index = repo.index().map_err(git_error)?;
    index.read(false).map_err(git_error)?;
    for file in files {
        let paths = delta_paths(file)?;
        refuse_conflicts(&index, file, &paths)?;
        for path in paths {
            if exists_no_follow(&workdir.join(path))? {
                index.add_path(path).map_err(git_error)?;
            } else if index.get_path(path, 0).is_some() {
                index.remove_path(path).map_err(git_error)?;
            }
        }
    }
    index.write().map_err(git_error)
}

/// Unstages the given files (see the module docs). Works on an unborn HEAD.
pub fn unstage_files(repo: &Repository, files: &[Delta]) -> Result<()> {
    ensure_workdir(repo)?;
    let mut head_index = git2::Index::new().map_err(git_error)?;
    if let Some(head) = head_commit(repo)? {
        head_index.read_tree(&head.tree().map_err(git_error)?).map_err(git_error)?;
    }
    let mut index = repo.index().map_err(git_error)?;
    index.read(false).map_err(git_error)?;
    for file in files {
        let paths = delta_paths(file)?;
        refuse_conflicts(&index, file, &paths)?;
        for path in paths {
            match head_index.get_path(path, 0) {
                Some(entry) => index.add(&entry).map_err(git_error)?,
                None if index.get_path(path, 0).is_some() => index.remove_path(path).map_err(git_error)?,
                None => {}
            }
        }
    }
    index.write().map_err(git_error)
}

/// Discards working-tree changes of the given files (see the module docs).
pub fn discard_files(repo: &Repository, files: &[Delta]) -> Result<()> {
    let workdir = ensure_workdir(repo)?.to_path_buf();
    let mut index = repo.index().map_err(git_error)?;
    index.read(false).map_err(git_error)?;
    let mut restore: Vec<PathBuf> = Vec::new();
    for file in files {
        let paths = delta_paths(file)?;
        refuse_conflicts(&index, file, &paths)?;
        for path in paths {
            if index.get_path(path, 0).is_some() {
                restore.push(path.to_path_buf());
            } else {
                remove_worktree_file(&workdir.join(path))?;
            }
        }
    }
    if restore.is_empty() {
        return Ok(());
    }
    let mut checkout = CheckoutBuilder::new();
    checkout.force().disable_pathspec_match(true).update_index(false);
    for path in &restore {
        checkout.path(path.as_path());
    }
    repo.checkout_index(Some(&mut index), Some(&mut checkout)).map_err(git_error)
}
