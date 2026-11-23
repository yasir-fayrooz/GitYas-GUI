//! Stash creation: whole-tree [`create_stash`] (popped with [`crate::stash::pop_stash`]) (used around fast-forward merges) and path-limited
//! [`stash_files`]. (Stash list / apply / drop are in the `stash` module.)
//!
//! **Why `stash_files` is plumbing, not `stash_save_ext` + `StashSaveOptions::pathspec`:** git2 0.20 does expose
//! `pathspec`, but libgit2 1.9's `git_stash_save_with_opts` with paths records *all* untracked files in the
//! untracked commit and then resets the **whole** working tree to HEAD (`reset_index_and_workdir` is not
//! path-limited), destroying changes in files the user did not select (see the test
//! `libgit2_pathspec_stash_destroys_unselected_changes`). git2's `pathspec()` also `unwrap()`s on paths with NUL.
//! So the stash is built here exactly like `git stash push [-u] -m <msg> -- <paths>`:
//! * `i` = full index tree, parent HEAD, message `index on <branch>: <short> <summary>`;
//! * `u` (only with `include_untracked` and selected untracked, non-ignored files) = tree of just those files, no
//!   parents, message `untracked files on <branch>: <short> <summary>`;
//! * `w` = index tree with each selected tracked path replaced by its working-tree state (deleted if missing),
//!   parents `[HEAD, i, u?]`, message `On <branch>: <msg>` (or `WIP on <branch>: <short> <summary>`);
//! * `refs/stash` → `w` with a reflog entry (that is the stash list);
//! * then only the selected paths are reset to HEAD in index and working tree (paths HEAD lacks are removed) and
//!   the stashed untracked files are deleted. Everything else is untouched.
//!
//! Matches the git CLI tree-for-tree (tested with `git stash push` as an oracle). If resetting fails after the
//! stash ref was written, nothing is lost: the content is in the stash.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use git2::build::CheckoutBuilder;
use git2::{ErrorCode, Repository, StashFlags};
use gityas_core::models::{CommitSignature, Delta};
use gityas_core::{GitYasError, Oid, Result};

use super::{delta_paths, exists_no_follow, remove_worktree_file};
use crate::commits::{head_commit, signature_now};
use crate::convert::oid_from_git;
use crate::git_error;
use crate::status::ensure_workdir;

const STASH_REF: &str = "refs/stash";
const MODE_BLOB: u32 = 0o100_644;
const MODE_EXEC: u32 = 0o100_755;
const MODE_LINK: u32 = 0o120_000;
const MODE_GITLINK: u32 = 0o160_000;

/// Stashes all local changes (`git stash push [-u] [-m msg]`). Returns `None` when there is nothing to stash.
/// Fails clearly on an unborn HEAD (git cannot stash before the first commit either) and an invalid signature.
pub fn create_stash(
    repo: &mut Repository,
    signature: &CommitSignature,
    message: Option<&str>,
    include_untracked: bool,
) -> Result<Option<Oid>> {
    ensure_workdir(repo)?;
    let sig = signature_now(signature)?;
    if head_commit(repo)?.is_none() {
        return Err(no_base_commit());
    }
    let flags = if include_untracked {
        StashFlags::INCLUDE_UNTRACKED
    } else {
        StashFlags::DEFAULT
    };
    let message = message.map(str::trim).filter(|m| !m.is_empty());
    match repo.stash_save2(&sig, message, Some(flags)) {
        Ok(oid) => Ok(Some(oid_from_git(oid)?)),
        // libgit2: "there is nothing to stash" (HEAD exists, checked above).
        Err(err) if err.code() == ErrorCode::NotFound => Ok(None),
        Err(err) => Err(git_error(err)),
    }
}

fn no_base_commit() -> GitYasError {
    GitYasError::InvalidInput("cannot stash before the first commit".into())
}

/// Stashes only `files` (see the module docs). Returns the new stash commit id.
pub fn stash_files(
    repo: &mut Repository,
    signature: &CommitSignature,
    files: &[Delta],
    message: Option<&str>,
    include_untracked: bool,
) -> Result<Oid> {
    let workdir = ensure_workdir(repo)?.to_path_buf();
    let sig = signature_now(signature)?;
    if files.is_empty() {
        return Err(GitYasError::InvalidInput("no files selected to stash".into()));
    }
    let mut selected: BTreeSet<PathBuf> = BTreeSet::new();
    for file in files {
        selected.extend(delta_paths(file)?.into_iter().map(Path::to_path_buf));
    }

    let base = head_commit(repo)?.ok_or_else(no_base_commit)?;
    let base_tree = base.tree().map_err(git_error)?;
    let mut base_index = git2::Index::new().map_err(git_error)?;
    base_index.read_tree(&base_tree).map_err(git_error)?;

    let mut index = repo.index().map_err(git_error)?;
    index.read(false).map_err(git_error)?;
    if index.has_conflicts() {
        return Err(GitYasError::InvalidInput("resolve merge conflicts before stashing".into()));
    }

    // Classify the selected paths.
    let mut tracked: Vec<&Path> = Vec::new();
    let mut untracked: Vec<&Path> = Vec::new();
    let mut changed = false;
    for path in &selected {
        let in_index = index.get_path(path, 0);
        if in_index.as_ref().is_some_and(|e| e.mode == MODE_GITLINK) || base_index.get_path(path, 0).is_some_and(|e| e.mode == MODE_GITLINK)
        {
            return Err(GitYasError::InvalidInput(format!("cannot stash submodule {}", path.display())));
        }
        if in_index.is_some() || base_index.get_path(path, 0).is_some() {
            tracked.push(path);
            changed |= !repo.status_file(path).map_err(git_error)?.is_empty();
        } else if include_untracked && exists_no_follow(&workdir.join(path))? && !repo.is_path_ignored(path).map_err(git_error)? {
            untracked.push(path);
            changed = true;
        }
    }
    if !changed {
        return Err(GitYasError::InvalidInput("the selected files have no changes to stash".into()));
    }

    let trust_filemode = match repo.config().and_then(|c| c.get_bool("core.filemode")) {
        Ok(value) => value,
        Err(err) if err.code() == ErrorCode::NotFound => true,
        Err(err) => return Err(git_error(err)),
    };
    let describe = describe_base(repo, &base)?;

    // i: the full index.
    let i_tree = repo.find_tree(index.write_tree().map_err(git_error)?).map_err(git_error)?;
    let i_commit = repo
        .commit(None, &sig, &sig, &format!("index on {describe}\n"), &i_tree, &[&base])
        .map_err(git_error)?;
    let i_commit = repo.find_commit(i_commit).map_err(git_error)?;

    // u: only the selected untracked files.
    let u_commit = if untracked.is_empty() {
        None
    } else {
        let mut u_index = git2::Index::new().map_err(git_error)?;
        for path in &untracked {
            u_index
                .add(&worktree_entry(repo, &workdir, path, None, trust_filemode)?)
                .map_err(git_error)?;
        }
        let u_tree = repo.find_tree(u_index.write_tree_to(repo).map_err(git_error)?).map_err(git_error)?;
        let oid = repo
            .commit(None, &sig, &sig, &format!("untracked files on {describe}\n"), &u_tree, &[])
            .map_err(git_error)?;
        Some(repo.find_commit(oid).map_err(git_error)?)
    };

    // w: index with the selected tracked paths taken from the working tree.
    let mut w_index = git2::Index::new().map_err(git_error)?;
    w_index.read_tree(&i_tree).map_err(git_error)?;
    for path in &tracked {
        if exists_no_follow(&workdir.join(path))? {
            let existing = w_index.get_path(path, 0).map(|e| e.mode);
            w_index
                .add(&worktree_entry(repo, &workdir, path, existing, trust_filemode)?)
                .map_err(git_error)?;
        } else if w_index.get_path(path, 0).is_some() {
            w_index.remove(path, 0).map_err(git_error)?;
        }
    }
    let w_tree = repo.find_tree(w_index.write_tree_to(repo).map_err(git_error)?).map_err(git_error)?;
    let w_message = match message.map(str::trim).filter(|m| !m.is_empty()) {
        Some(msg) => format!("On {}: {msg}", branch_label(repo)?),
        None => format!("WIP on {describe}"),
    };
    let mut parents = vec![&base, &i_commit];
    if let Some(u) = &u_commit {
        parents.push(u);
    }
    let w_commit = repo
        .commit(None, &sig, &sig, &format!("{w_message}\n"), &w_tree, &parents)
        .map_err(git_error)?;
    repo.reference_ensure_log(STASH_REF).map_err(git_error)?;
    repo.reference(STASH_REF, w_commit, true, &w_message).map_err(git_error)?;

    // Reset only the selected paths.
    let mut in_base: Vec<&Path> = Vec::new();
    let mut not_in_base: Vec<&Path> = Vec::new();
    for path in &tracked {
        if base_index.get_path(path, 0).is_some() {
            in_base.push(path);
        } else {
            not_in_base.push(path);
            if index.get_path(path, 0).is_some() {
                index.remove_path(path).map_err(git_error)?;
            }
        }
    }
    index.write().map_err(git_error)?;
    if !in_base.is_empty() {
        let mut checkout = CheckoutBuilder::new();
        checkout.force().disable_pathspec_match(true);
        for path in &in_base {
            checkout.path(*path);
        }
        repo.checkout_tree(base_tree.as_object(), Some(&mut checkout)).map_err(git_error)?;
    }
    for path in not_in_base.iter().chain(untracked.iter()) {
        remove_worktree_file(&workdir.join(path))?;
    }
    oid_from_git(w_commit)
}

/// `<branch>: <short id> <summary>` as git writes it in stash messages.
fn describe_base(repo: &Repository, base: &git2::Commit<'_>) -> Result<String> {
    let short = base.as_object().short_id().map_err(git_error)?;
    let short = short.as_str().unwrap_or_default().to_owned();
    let summary = base
        .summary_bytes()
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .unwrap_or_default();
    Ok(format!("{}: {short} {summary}", branch_label(repo)?))
}

fn branch_label(repo: &Repository) -> Result<String> {
    if repo.head_detached().map_err(git_error)? {
        return Ok("(no branch)".into());
    }
    let head = repo.head().map_err(git_error)?;
    Ok(String::from_utf8_lossy(head.shorthand_bytes()).into_owned())
}

/// An index entry for the working-tree file at `path`: blob through the repository's filters, mode from the file
/// (symlink / executable), or `existing_mode` when `core.filemode` is false.
fn worktree_entry(
    repo: &Repository,
    workdir: &Path,
    path: &Path,
    existing_mode: Option<u32>,
    trust_filemode: bool,
) -> Result<git2::IndexEntry> {
    let full = workdir.join(path);
    let meta = std::fs::symlink_metadata(&full)?;
    let (id, mode) = if meta.file_type().is_symlink() {
        let target = std::fs::read_link(&full)?;
        let id = repo.blob(target.as_os_str().as_encoded_bytes()).map_err(git_error)?;
        (id, MODE_LINK)
    } else if meta.is_file() {
        let id = repo.blob_path(&full).map_err(git_error)?;
        let mode = if trust_filemode {
            if is_executable(&meta) { MODE_EXEC } else { MODE_BLOB }
        } else {
            existing_mode.filter(|m| *m == MODE_EXEC || *m == MODE_BLOB).unwrap_or(MODE_BLOB)
        };
        (id, mode)
    } else {
        return Err(GitYasError::InvalidInput(format!(
            "cannot stash {}: not a regular file",
            path.display()
        )));
    };
    Ok(git2::IndexEntry {
        ctime: git2::IndexTime::new(0, 0),
        mtime: git2::IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        file_size: 0,
        id,
        flags: 0,
        flags_extended: 0,
        path: index_path_bytes(path)?,
    })
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}

/// Index paths are bytes with `/` separators on every platform.
fn index_path_bytes(path: &Path) -> Result<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(path.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        let s = crate::convert::path_to_str(path)?;
        Ok(s.replace('\\', "/").into_bytes())
    }
}
