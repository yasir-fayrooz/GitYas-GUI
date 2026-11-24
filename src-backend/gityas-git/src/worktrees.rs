//! Linked worktrees (`git worktree list / add / remove`). Not the working-tree *files* (that is [`crate::worktree`]).
//!
//! Works from any worktree of the repository: the list always comes from the main repository (the common dir), so
//! the main worktree and every linked one are listed whichever of them is open.

use std::path::{Path, PathBuf};

use git2::{BranchType, ErrorCode, Repository, StatusOptions, WorktreeAddOptions, WorktreeLockStatus, WorktreePruneOptions};
use gityas_core::models::{Worktree, WorktreeBranch};
use gityas_core::{GitYasError, Result};

use crate::convert::oid_from_git;
use crate::git_error;

/// The main repository (the one owning the common dir); `repo` itself unless it is a linked worktree.
fn main_repository(repo: &Repository) -> Result<Repository> {
    if !repo.is_worktree() {
        return Repository::open(repo.path()).map_err(git_error);
    }
    Repository::open(repo.commondir()).map_err(git_error)
}

/// Same folder, ignoring `\\?\` prefixes, symlinks and trailing separators where the paths still exist.
fn same_path(a: &Path, b: &Path) -> bool {
    match (dunce_canonical(a), dunce_canonical(b)) {
        (Some(a), Some(b)) => a == b,
        _ => strip_trailing(a) == strip_trailing(b),
    }
}

fn dunce_canonical(path: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    // Windows `canonicalize` returns `\\?\C:\…`; compare without the verbatim prefix.
    let text = canonical.to_string_lossy();
    Some(match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => canonical,
    })
}

fn strip_trailing(path: &Path) -> PathBuf {
    path.components().collect()
}

/// `(branch short name, head commit)` of the repository at a worktree.
fn head_of(repo: &Repository) -> (Option<String>, Option<gityas_core::Oid>) {
    let branch = repo
        .find_reference("HEAD")
        .ok()
        .and_then(|h| h.symbolic_target().map(str::to_owned))
        .and_then(|target| target.strip_prefix("refs/heads/").map(str::to_owned));
    let head = repo.head().ok().and_then(|h| h.target()).and_then(|oid| oid_from_git(oid).ok());
    (branch, head)
}

/// Every worktree: the main one first (absent for a bare repository), then the linked ones by name.
pub fn list_worktrees(repo: &Repository) -> Result<Vec<Worktree>> {
    let main = main_repository(repo)?;
    let current = repo.workdir().map(Path::to_path_buf);
    let is_current = |path: &Path| current.as_deref().is_some_and(|c| same_path(c, path));
    let mut out = Vec::new();
    if let Some(workdir) = main.workdir() {
        let (branch, head) = head_of(&main);
        out.push(Worktree {
            name: None,
            path: strip_trailing(workdir),
            branch,
            head,
            is_current: is_current(workdir),
            locked: false,
            lock_reason: None,
            prunable: false,
        });
    }
    let names = main.worktrees().map_err(git_error)?;
    let mut names: Vec<&str> = names.iter().flatten().collect();
    names.sort_unstable();
    for name in names {
        let wt = match main.find_worktree(name) {
            Ok(wt) => wt,
            Err(err) => {
                tracing::debug!(worktree = name, "unreadable worktree: {err}");
                continue;
            }
        };
        let path = strip_trailing(wt.path());
        let (locked, lock_reason) = match wt.is_locked() {
            Ok(WorktreeLockStatus::Locked(reason)) => (true, reason.filter(|r| !r.trim().is_empty())),
            _ => (false, None),
        };
        let prunable = wt.validate().is_err();
        let (branch, head) = match Repository::open_from_worktree(&wt) {
            Ok(wt_repo) if !prunable => head_of(&wt_repo),
            _ => (None, None),
        };
        out.push(Worktree {
            name: Some(name.to_owned()),
            is_current: !prunable && is_current(&path),
            path,
            branch,
            head,
            locked,
            lock_reason,
            prunable,
        });
    }
    Ok(out)
}

/// A worktree name (`.git/worktrees/<name>`) for `path`: its folder name made ref-safe, unique among the existing
/// ones (`name`, `name-2`, …).
fn worktree_name(main: &Repository, path: &Path) -> Result<String> {
    let folder = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| GitYasError::InvalidInput(format!("{} has no folder name", path.display())))?;
    let mut base: String = folder
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    base = base.trim_matches(['-', '.']).to_owned();
    if base.is_empty() {
        base = "worktree".to_owned();
    }
    let existing = main.worktrees().map_err(git_error)?;
    let taken = |n: &str| existing.iter().flatten().any(|e| e == n) || main.path().join("worktrees").join(n).exists();
    let mut name = base.clone();
    let mut i = 2;
    while taken(&name) {
        name = format!("{base}-{i}");
        i += 1;
    }
    Ok(name)
}

/// Adds a linked worktree at `path` (a folder that doesn't exist yet, or is empty) checking out `branch`. A new
/// branch is created first and deleted again if the worktree can't be added. Returns the new worktree.
pub fn add_worktree(repo: &Repository, path: &Path, branch: &WorktreeBranch) -> Result<Worktree> {
    let main = main_repository(repo)?;
    if path.exists() && std::fs::read_dir(path).map(|mut d| d.next().is_some()).unwrap_or(true) {
        return Err(GitYasError::AlreadyExists(format!("{} is not empty", path.display())));
    }
    let name = worktree_name(&main, path)?;
    let (refname, created) = match branch {
        WorktreeBranch::Existing { refname } => {
            let short = refname
                .strip_prefix("refs/heads/")
                .ok_or_else(|| GitYasError::InvalidInput(format!("{refname:?} is not a local branch")))?;
            main.find_branch(short, BranchType::Local).map_err(|e| match e.code() {
                ErrorCode::NotFound => GitYasError::NotFound(format!("branch {short:?} does not exist")),
                _ => git_error(e),
            })?;
            if let Some(other) = list_worktrees(repo)?.into_iter().find(|w| w.branch.as_deref() == Some(short)) {
                return Err(GitYasError::InvalidInput(format!(
                    "{short:?} is already checked out in {}",
                    other.path.display()
                )));
            }
            (refname.clone(), false)
        }
        WorktreeBranch::New { name, from } => (crate::branches::create_branch(repo, name, from.as_ref(), false)?, true),
    };
    let result = (|| {
        // libgit2 creates the worktree folder but not its parents.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let reference = main.find_reference(&refname).map_err(git_error)?;
        let mut opts = WorktreeAddOptions::new();
        opts.reference(Some(&reference));
        main.worktree(&name, path, Some(&opts)).map_err(git_error)
    })();
    if let Err(err) = result {
        if created && let Ok(mut b) = main.find_branch(&refname["refs/heads/".len()..], BranchType::Local) {
            let _ = b.delete();
        }
        return Err(err);
    }
    list_worktrees(repo)?
        .into_iter()
        .find(|w| w.name.as_deref() == Some(name.as_str()))
        .ok_or_else(|| GitYasError::Other(format!("worktree {name:?} was added but is not listed")))
}

/// Whether the worktree at `wt_repo` has changes `git worktree remove` would refuse to discard (modified, staged or
/// untracked files; ignored files don't count).
fn is_dirty(wt_repo: &Repository) -> Result<bool> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true).include_ignored(false).exclude_submodules(true);
    Ok(!wt_repo.statuses(Some(&mut opts)).map_err(git_error)?.is_empty())
}

/// Removes linked worktree `name`: deletes its folder and its admin data (`git worktree remove`). A worktree whose
/// folder is already gone is just pruned. Without `force`, refuses a worktree with uncommitted changes
/// ([`GitYasError::WorktreeDirty`]) or a locked one. The main worktree and the open one can't be removed. The
/// branch it had checked out is kept.
pub fn remove_worktree(repo: &Repository, name: &str, force: bool) -> Result<()> {
    let main = main_repository(repo)?;
    let wt = main.find_worktree(name).map_err(|e| match e.code() {
        ErrorCode::NotFound => GitYasError::NotFound(format!("worktree {name:?} does not exist")),
        _ => git_error(e),
    })?;
    let folder = wt
        .path()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_owned());
    if let Some(current) = repo.workdir()
        && same_path(current, wt.path())
    {
        return Err(GitYasError::InvalidInput(format!(
            "{folder:?} is the open worktree; open another one to remove it"
        )));
    }
    let valid = wt.validate().is_ok();
    if let Ok(WorktreeLockStatus::Locked(reason)) = wt.is_locked()
        && !force
    {
        let why = reason
            .filter(|r| !r.trim().is_empty())
            .map(|r| format!(" ({})", r.trim()))
            .unwrap_or_default();
        return Err(GitYasError::InvalidInput(format!(
            "{folder:?} is locked{why}; unlock it or remove it with force"
        )));
    }
    if valid && !force {
        let wt_repo = Repository::open_from_worktree(&wt).map_err(git_error)?;
        if is_dirty(&wt_repo)? {
            return Err(GitYasError::WorktreeDirty(folder));
        }
    }
    let mut opts = WorktreePruneOptions::new();
    opts.valid(true).locked(true).working_tree(true);
    wt.prune(Some(&mut opts)).map_err(git_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_trailing_drops_separators() {
        assert_eq!(strip_trailing(Path::new("a/b/")), PathBuf::from("a/b"));
    }
}
