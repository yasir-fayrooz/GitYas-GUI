//! List / create / delete / checkout branches, upstream ahead/behind, create remote branch.
//!
//! Commands take full ref names (`refs/heads/x`, `refs/remotes/origin/x`), so remote names containing `/` are
//! never guessed by splitting on the first slash: libgit2 resolves them from the configured refspecs.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use git2::{BranchType, ErrorCode, Repository};
use gityas_core::models::{Branch, BranchKind};
use gityas_core::{GitYasError, Oid, Result};

use crate::convert::{oid_from_git, oid_to_git};
use crate::git_error;
use crate::remote::{RemoteContext, push_refspecs};
use crate::repository::{ensure_clean_state, ensure_not_bare, fresh_index, head_tree, ref_exists};

/// What [`checkout_branch`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutOutcome {
    /// HEAD now points at this local branch (full ref name).
    Switched { refname: String },
    /// A remote branch was checked out: a local tracking branch was created (or an existing one tracking it reused).
    Tracking { refname: String, upstream: String, created: bool },
}

/// Lists branches (`kind = None`: local then remote). Symbolic remote refs (`<remote>/HEAD`, for any remote) are
/// hidden. Branches with non-UTF-8 names are skipped with a warning (commands couldn't address them by name).
///
/// Local branches carry upstream + ahead/behind. A configured upstream whose remote-tracking ref no longer exists
/// is reported with `upstream_gone: true` (`upstream`/`upstream_refname` still name it, ahead/behind 0).
pub fn list_branches(repo: &Repository, kind: Option<BranchKind>) -> Result<Vec<Branch>> {
    let filter = kind.map(|k| match k {
        BranchKind::Local => BranchType::Local,
        BranchKind::Remote => BranchType::Remote,
    });
    let mut out = Vec::new();
    for item in repo.branches(filter).map_err(git_error)? {
        let (branch, branch_type) = item.map_err(git_error)?;
        let reference = branch.get();
        if reference.kind() == Some(git2::ReferenceType::Symbolic) {
            continue;
        }
        let Some(refname) = reference.name().map(str::to_owned) else {
            tracing::warn!(name = %String::from_utf8_lossy(reference.name_bytes()), "skipping branch with a non-UTF-8 name");
            continue;
        };
        let Some(name) = reference.shorthand().map(str::to_owned) else {
            continue;
        };
        let target = reference.target().map(oid_from_git).transpose()?;
        let mut b = Branch {
            name,
            refname: refname.clone(),
            kind: match branch_type {
                BranchType::Local => BranchKind::Local,
                BranchType::Remote => BranchKind::Remote,
            },
            target,
            remote_name: None,
            upstream: None,
            upstream_refname: None,
            upstream_gone: false,
            ahead: 0,
            behind: 0,
            is_head: branch.is_head(),
        };
        match branch_type {
            BranchType::Remote => b.remote_name = remote_of_tracking_ref(repo, &refname)?,
            BranchType::Local => {
                if let Some(up_name) = crate::remote::upstream_refname(repo, &refname)? {
                    b.upstream_refname = Some(up_name.clone());
                    match repo.find_reference(&up_name) {
                        Ok(up) => {
                            let up = up.resolve().map_err(git_error)?;
                            b.upstream = up.shorthand().map(str::to_owned);
                            b.remote_name = remote_of_tracking_ref(repo, &up_name)?;
                            if let (Some(local), Some(remote)) = (reference.target(), up.target()) {
                                let (ahead, behind) = repo.graph_ahead_behind(local, remote).map_err(git_error)?;
                                b.ahead = ahead;
                                b.behind = behind;
                            }
                        }
                        Err(e) if e.code() == ErrorCode::NotFound => {
                            tracing::debug!(branch = %refname, upstream = %up_name, "upstream is gone");
                            b.upstream_gone = true;
                            b.upstream = Some(gityas_core::models::shorten_refname(&up_name).to_owned());
                            b.remote_name = remote_of_tracking_ref(repo, &up_name)?;
                        }
                        Err(e) => return Err(git_error(e)),
                    }
                }
            }
        }
        out.push(b);
    }
    Ok(out)
}

/// Remote owning a remote-tracking ref, from the configured fetch refspecs (handles remote names with `/`).
fn remote_of_tracking_ref(repo: &Repository, refname: &str) -> Result<Option<String>> {
    match repo.branch_remote_name(refname) {
        Ok(buf) => Ok(buf.as_str().map(str::to_owned)),
        // No remote's refspec maps to it (stale ref after a remote was removed) or it's ambiguous.
        Err(e) if matches!(e.code(), ErrorCode::NotFound | ErrorCode::Ambiguous) => Ok(None),
        Err(e) => Err(git_error(e)),
    }
}

fn validate_branch_name(name: &str) -> Result<()> {
    if name.contains('\0') || !git2::Branch::name_is_valid(name).map_err(git_error)? {
        return Err(GitYasError::InvalidInput(format!("{name:?} is not a valid branch name")));
    }
    Ok(())
}

/// Creates local branch `name` at `start` (default: HEAD's commit) and optionally checks it out.
/// Returns the new full ref name. Fails on an unborn HEAD without `start`, an existing branch, or an invalid name.
pub fn create_branch(repo: &Repository, name: &str, start: Option<&Oid>, checkout: bool) -> Result<String> {
    validate_branch_name(name)?;
    let head_commit = match repo.head() {
        Ok(h) => Some(h.peel_to_commit().map_err(git_error)?),
        Err(e) if matches!(e.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => None,
        Err(e) => return Err(git_error(e)),
    };
    let commit = match start {
        Some(oid) => repo.find_commit(oid_to_git(oid)?).map_err(|e| match e.code() {
            ErrorCode::NotFound => GitYasError::NotFound(format!("commit {oid} does not exist")),
            _ => git_error(e),
        })?,
        None => head_commit
            .clone()
            .ok_or_else(|| GitYasError::InvalidInput("cannot create a branch before the first commit".to_owned()))?,
    };
    let branch = repo.branch(name, &commit, false).map_err(|e| match e.code() {
        ErrorCode::Exists => GitYasError::AlreadyExists(format!("a branch named {name:?}")),
        _ => git_error(e),
    })?;
    let refname = format!("refs/heads/{name}");
    if checkout {
        let same_commit = head_commit.as_ref().is_some_and(|h| h.id() == commit.id());
        let result = if same_commit {
            // Same tree: just move HEAD, keeping any local changes (like `git switch -c`).
            ensure_clean_state(repo, "switch branches").and_then(|()| repo.set_head(&refname).map_err(git_error))
        } else {
            checkout_branch(repo, &refname).map(|_| ())
        };
        if let Err(e) = result {
            let mut branch = branch;
            if let Err(del) = branch.delete() {
                tracing::warn!(error = del.message(), "could not roll back branch after failed checkout");
            }
            return Err(e);
        }
    }
    Ok(refname)
}

/// Files with local changes (modified, deleted or untracked) that also differ between `from` and `to`: the ones a
/// checkout/merge from `from` to `to` would overwrite. Used to name the files when libgit2 refuses without saying.
pub(crate) fn blocking_paths(repo: &Repository, from: Option<&git2::Tree<'_>>, to: &git2::Tree<'_>) -> Result<Vec<PathBuf>> {
    let diff = repo.diff_tree_to_tree(from, Some(to), None).map_err(git_error)?;
    let mut changed = std::collections::BTreeSet::new();
    for delta in diff.deltas() {
        for file in [delta.old_file(), delta.new_file()] {
            if let Some(p) = file.path() {
                changed.insert(p.to_path_buf());
            }
        }
    }
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true).include_ignored(false);
    let statuses = repo.statuses(Some(&mut opts)).map_err(git_error)?;
    let mut out = Vec::new();
    for entry in statuses.iter() {
        if entry.status().is_empty() || entry.status() == git2::Status::CURRENT {
            continue;
        }
        let path = crate::repository::path_from_bytes(entry.path_bytes())?;
        if changed.contains(&path) {
            out.push(path);
        }
    }
    Ok(out)
}

/// Checks out `tree_obj` without overwriting local changes: non-conflicting changes are carried over, otherwise
/// fails listing the files and leaves the working tree untouched.
///
/// A directory another process has open (a shell's or editor's current directory on Windows) cannot be removed, and
/// libgit2 then aborts the whole checkout at the first file it deletes from it, even if the directory is not empty.
/// Like git, we leave such directories behind: the checkout is retried with the deletions under them excluded, and
/// those files are deleted here, without removing their directories. (libgit2's
/// `GIT_CHECKOUT_SKIP_LOCKED_DIRECTORIES` would do this, but git2 has no safe setter for it.)
///
/// Tracked files behind a directory junction are kept away from libgit2 too (see `linked_dir_plan`).
pub(crate) fn safe_checkout(repo: &Repository, tree_obj: &git2::Object<'_>, action: &str) -> Result<()> {
    #[cfg(windows)]
    let (mut held_back, untouched) = linked_dir_plan(repo, tree_obj, action)?;
    #[cfg(not(windows))]
    let (mut held_back, untouched) = (BTreeSet::<String>::new(), BTreeSet::<String>::new());
    loop {
        let blocked = RefCell::new(Vec::<PathBuf>::new());
        let touched = RefCell::new(Vec::<PathBuf>::new());
        let mut cb = git2::build::CheckoutBuilder::new();
        cb.safe();
        if !held_back.is_empty() || !untouched.is_empty() {
            // The first matching pathspec wins, so the exclusions go before the catch-all.
            for path in held_back.iter().chain(&untouched) {
                cb.path(format!("!{path}"));
            }
            cb.path("*");
        }
        cb.notify_on(git2::CheckoutNotificationType::CONFLICT);
        cb.notify(|_, path, _, _, _| {
            if let Some(p) = path {
                blocked.borrow_mut().push(p.to_path_buf());
            }
            true
        });
        // Paths libgit2 got to before failing: its removals (deleted files, and files it rewrites by removing them
        // first, like an executable on Windows) all happen before any update.
        cb.progress(|path, _, _| {
            if let Some(p) = path {
                touched.borrow_mut().push(p.to_path_buf());
            }
        });
        let result = repo.checkout_tree(tree_obj, Some(&mut cb));
        drop(cb);
        let Err(e) = result else {
            return remove_held_back(repo, &held_back);
        };
        // A checkout that stops part way (a locked directory) has already removed files: put them back, so the
        // retry does not take them for local deletions and a failure leaves the working tree as it was.
        restore_removed(repo, &touched.into_inner())?;
        let blocked = blocked.into_inner();
        if blocked.is_empty()
            && let Some(dir) = locked_dir(repo, &e)
        {
            // Conflicts are checked before anything is removed, so every deletion was already found safe.
            let before = held_back.len();
            held_back.extend(deletions_under(repo, tree_obj, &dir)?);
            if held_back.len() > before {
                tracing::warn!(dir, "cannot remove directory (in use by another process); leaving it behind");
                continue;
            }
        }
        return checkout_error(repo, tree_obj, action, e, blocked);
    }
}

/// libgit2 on Windows takes a directory junction for a symlink, so for tracked files behind one (a
/// `.claude/skills/x -> .agents/skills/x` link from a tool installer) it sees a file where the trees have a
/// directory and refuses any checkout that touches them. Git for Windows walks junctions like directories, so
/// these files are kept out of libgit2's checkout and handled as git would:
/// * unchanged between the index and the target: left alone (`untouched`);
/// * deleted by the target: deleted through the link after the checkout (`held_back`, see [`remove_held_back`]),
///   unless they have local changes, which refuses the checkout naming them;
/// * added or changed by the target: refused, since writing them through the link needs libgit2's filters.
#[cfg(windows)]
fn linked_dir_plan(repo: &Repository, tree_obj: &git2::Object<'_>, action: &str) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    use crate::status::{index_stage, tracked_file_changed, tracked_linked_dirs};

    let (mut held_back, mut untouched) = (BTreeSet::new(), BTreeSet::new());
    let Some(workdir) = repo.workdir() else {
        return Ok((held_back, untouched));
    };
    let index = fresh_index(repo)?;
    let links = tracked_linked_dirs(workdir, &index);
    if links.is_empty() {
        return Ok((held_back, untouched));
    }
    let to = tree_obj.peel_to_tree().map_err(git_error)?;
    let mut blocked = Vec::new();
    for link in links {
        let link_str = link.to_string_lossy().replace('\\', "/");
        let prefix = format!("{link_str}/");
        let mut target = std::collections::BTreeMap::<String, git2::Oid>::new();
        if let Ok(subtree) = to.get_path(&link).and_then(|e| e.to_object(repo)).and_then(|o| o.peel_to_tree()) {
            subtree
                .walk(git2::TreeWalkMode::PreOrder, |root, entry| {
                    if entry.kind() == Some(git2::ObjectType::Blob)
                        && let Some(name) = entry.name()
                    {
                        target.insert(format!("{prefix}{root}{name}"), entry.id());
                    }
                    git2::TreeWalkResult::Ok
                })
                .map_err(git_error)?;
        }
        for entry in index
            .iter()
            .filter(|e| index_stage(e) == 0 && e.path.starts_with(prefix.as_bytes()))
        {
            let path = String::from_utf8_lossy(&entry.path).into_owned();
            match target.remove(&path) {
                Some(oid) if oid == entry.id => {
                    untouched.insert(path);
                }
                Some(_) => return Err(linked_dir_unsupported(action, &link_str)),
                None => {
                    let rel = PathBuf::from(&path);
                    let changed = match std::fs::metadata(workdir.join(&rel)) {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                        Err(e) => return Err(e.into()),
                        Ok(meta) => meta.is_dir() || tracked_file_changed(repo, workdir, &entry, &rel, &meta)?,
                    };
                    if changed {
                        blocked.push(rel);
                    }
                    held_back.insert(path);
                }
            }
        }
        if !target.is_empty() {
            return Err(linked_dir_unsupported(action, &link_str));
        }
    }
    if !blocked.is_empty() {
        return Err(crate::remote::merge_flow_overwrite_error(action, &blocked));
    }
    Ok((held_back, untouched))
}

#[cfg(windows)]
fn linked_dir_unsupported(action: &str, link: &str) -> GitYasError {
    GitYasError::InvalidInput(format!(
        "cannot {action}: it changes files inside {link}, which is a directory junction; replace the junction with a \
         regular folder and try again"
    ))
}

fn checkout_error(repo: &Repository, tree_obj: &git2::Object<'_>, action: &str, e: git2::Error, mut blocked: Vec<PathBuf>) -> Result<()> {
    if !blocked.is_empty() || e.code() == ErrorCode::Conflict {
        if blocked.is_empty() {
            let head_tree = head_tree(repo)?;
            let to = tree_obj.peel_to_tree().map_err(git_error)?;
            blocked = blocking_paths(repo, head_tree.as_ref(), &to)?;
        }
        Err(crate::remote::merge_flow_overwrite_error(action, &blocked))
    } else {
        Err(git_error(e))
    }
}

/// Rewrites from the index the tracked files among `paths` that are missing from the working tree. Used after a
/// failed checkout on the paths it processed: a safe checkout only removes files that match the index, so the index
/// holds exactly what was there.
fn restore_removed(repo: &Repository, paths: &[PathBuf]) -> Result<()> {
    let Some(workdir) = repo.workdir() else {
        return Ok(());
    };
    let mut index = fresh_index(repo)?;
    let missing: Vec<&PathBuf> = paths
        .iter()
        .filter(|p| index.get_path(p, 0).is_some() && std::fs::symlink_metadata(workdir.join(p)).is_err())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    tracing::debug!(count = missing.len(), "restoring files removed by a failed checkout");
    let mut cb = git2::build::CheckoutBuilder::new();
    cb.force().disable_pathspec_match(true);
    for p in missing {
        cb.path(p);
    }
    repo.checkout_index(Some(&mut index), Some(&mut cb)).map_err(git_error)
}

/// The workdir-relative directory (`a/b`) of a libgit2 "could not rmdir" error.
fn locked_dir(repo: &Repository, e: &git2::Error) -> Option<String> {
    if e.class() != git2::ErrorClass::Os {
        return None;
    }
    let dir = e.message().strip_prefix("could not rmdir '")?.split('\'').next()?;
    let workdir = repo.workdir()?.to_string_lossy().replace('\\', "/");
    let rel = dir.strip_prefix(workdir.trim_end_matches('/'))?.trim_matches('/');
    (!rel.is_empty()).then(|| rel.to_owned())
}

/// Files under `dir` that HEAD has and `target` does not: the deletions that make libgit2 try to remove `dir`.
fn deletions_under(repo: &Repository, target: &git2::Object<'_>, dir: &str) -> Result<Vec<String>> {
    let Some(from) = head_tree(repo)? else {
        return Ok(Vec::new());
    };
    let to = target.peel_to_tree().map_err(git_error)?;
    let mut opts = git2::DiffOptions::new();
    opts.pathspec(dir).disable_pathspec_match(false);
    let diff = repo.diff_tree_to_tree(Some(&from), Some(&to), Some(&mut opts)).map_err(git_error)?;
    let prefix = format!("{dir}/");
    Ok(diff
        .deltas()
        .filter(|d| d.status() == git2::Delta::Deleted)
        .filter_map(|d| d.old_file().path().and_then(|p| p.to_str()).map(|p| p.replace('\\', "/")))
        .filter(|p| p.starts_with(&prefix))
        .collect())
}

/// Finishes the deletions [`safe_checkout`] held back: removes the files (libgit2 may already have), the directories
/// that leaves empty where possible, and the index entries. Locked directories stay, as git leaves them.
fn remove_held_back(repo: &Repository, paths: &BTreeSet<String>) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let workdir = repo
        .workdir()
        .ok_or_else(|| GitYasError::InvalidInput("cannot check out in a bare repository".to_owned()))?;
    let mut index = fresh_index(repo)?;
    for path in paths {
        let full = workdir.join(path);
        match std::fs::remove_file(&full) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(GitYasError::Io(format!("cannot remove {}: {e}", full.display()))),
        }
        index.remove_path(Path::new(path)).map_err(git_error)?;
    }
    // Deepest first, so a directory is emptied before its parent is tried.
    for path in paths.iter().rev() {
        for dir in Path::new(path).ancestors().skip(1).take_while(|d| !d.as_os_str().is_empty()) {
            // `remove_dir` drops a directory junction even while its target still has files; keep those, as git does.
            let full = workdir.join(dir);
            if std::fs::read_dir(&full).is_ok_and(|mut d| d.next().is_some()) || std::fs::remove_dir(&full).is_err() {
                break;
            }
        }
    }
    index.write().map_err(git_error)
}

fn ensure_checkout_allowed(repo: &Repository) -> Result<()> {
    ensure_not_bare(repo, "check out")?;
    ensure_clean_state(repo, "check out")?;
    if fresh_index(repo)?.has_conflicts() {
        return Err(GitYasError::InvalidInput("resolve the conflicts in the index first".to_owned()));
    }
    Ok(())
}

/// Checks out a branch by full ref name.
/// * `refs/heads/x`: switch to it.
/// * `refs/remotes/<remote>/x` (or a symbolic `<remote>/HEAD`): switch to local `x` if it already tracks that
///   remote branch, otherwise create `x` there with that upstream. A local `x` tracking something else is an error.
///
/// A dirty working tree is never clobbered: changes in files that differ between the two commits make the
/// checkout fail with the list of files (nothing is modified); other local changes are carried over.
pub fn checkout_branch(repo: &Repository, refname: &str) -> Result<CheckoutOutcome> {
    ensure_checkout_allowed(repo)?;
    let reference = repo
        .find_reference(refname)
        .map_err(|e| match e.code() {
            ErrorCode::NotFound => GitYasError::NotFound(format!("branch {refname:?} does not exist")),
            _ => git_error(e),
        })?
        .resolve()
        .map_err(git_error)?;
    let resolved = reference
        .name()
        .ok_or_else(|| GitYasError::InvalidInput("branch name is not valid UTF-8".to_owned()))?
        .to_owned();
    let commit = reference.peel_to_commit().map_err(git_error)?;

    if resolved.starts_with("refs/heads/") {
        safe_checkout(repo, commit.as_object(), "checkout")?;
        repo.set_head(&resolved).map_err(git_error)?;
        return Ok(CheckoutOutcome::Switched { refname: resolved });
    }
    if !resolved.starts_with("refs/remotes/") {
        return Err(GitYasError::InvalidInput(format!("{refname:?} is not a branch")));
    }
    let remote = remote_of_tracking_ref(repo, &resolved)?
        .ok_or_else(|| GitYasError::InvalidInput(format!("{resolved:?} does not belong to a configured remote")))?;
    let local_name = resolved
        .strip_prefix(&format!("refs/remotes/{remote}/"))
        .filter(|n| !n.is_empty())
        .ok_or_else(|| GitYasError::InvalidInput(format!("cannot derive a local branch name from {resolved:?}")))?
        .to_owned();
    let local_ref = format!("refs/heads/{local_name}");
    let upstream_short = format!("{remote}/{local_name}");

    match repo.find_branch(&local_name, BranchType::Local) {
        Ok(_) => {
            let tracks = crate::remote::upstream_refname(repo, &local_ref)?;
            if tracks.as_deref() != Some(resolved.as_str()) {
                return Err(GitYasError::InvalidInput(format!(
                    "a local branch {local_name:?} already exists and does not track {upstream_short}"
                )));
            }
            // Existing tracking branch: switch to it as is (like `git switch`; pulling is separate).
            let local_commit = repo
                .find_reference(&local_ref)
                .and_then(|r| r.peel_to_commit())
                .map_err(git_error)?;
            safe_checkout(repo, local_commit.as_object(), "checkout")?;
            repo.set_head(&local_ref).map_err(git_error)?;
            Ok(CheckoutOutcome::Tracking {
                refname: local_ref,
                upstream: upstream_short,
                created: false,
            })
        }
        Err(e) if e.code() == ErrorCode::NotFound => {
            validate_branch_name(&local_name)?;
            let mut branch = repo.branch(&local_name, &commit, false).map_err(git_error)?;
            let switched = branch
                .set_upstream(Some(&upstream_short))
                .map_err(git_error)
                .and_then(|()| safe_checkout(repo, commit.as_object(), "checkout"))
                .and_then(|()| repo.set_head(&local_ref).map_err(git_error));
            if let Err(e) = switched {
                if let Err(del) = branch.delete() {
                    tracing::warn!(error = del.message(), "could not roll back tracking branch after failed checkout");
                }
                return Err(e);
            }
            Ok(CheckoutOutcome::Tracking {
                refname: local_ref,
                upstream: upstream_short,
                created: true,
            })
        }
        Err(e) => Err(git_error(e)),
    }
}

/// Detaches HEAD at `commit` (same dirty-tree rules as [`checkout_branch`]).
pub fn checkout_commit(repo: &Repository, commit: &Oid) -> Result<()> {
    ensure_checkout_allowed(repo)?;
    let commit = repo.find_commit(oid_to_git(commit)?).map_err(|e| match e.code() {
        ErrorCode::NotFound => GitYasError::NotFound(format!("commit {commit} does not exist")),
        _ => git_error(e),
    })?;
    safe_checkout(repo, commit.as_object(), "checkout")?;
    repo.set_head_detached(commit.id()).map_err(git_error)
}

/// Deletes a local branch. Without `force`, refuses unless the branch is merged into its upstream (if it has one)
/// or into HEAD (like `git branch -d`). The checked-out branch (here or in another worktree) is never deleted.
/// Renames local branch `refname` (`refs/heads/x`) to `new_name` and returns the new full ref name. Like
/// `git branch -m`: HEAD follows when it is the checked-out branch, and its config (upstream) and reflog move with it.
/// Fails on an invalid name or an existing branch; renaming to the current name is a no-op.
pub fn rename_local_branch(repo: &Repository, refname: &str, new_name: &str) -> Result<String> {
    let short = refname
        .strip_prefix("refs/heads/")
        .ok_or_else(|| GitYasError::InvalidInput(format!("{refname:?} is not a local branch")))?;
    validate_branch_name(new_name)?;
    let mut branch = repo.find_branch(short, BranchType::Local).map_err(|e| match e.code() {
        ErrorCode::NotFound => GitYasError::NotFound(format!("branch {short:?} does not exist")),
        _ => git_error(e),
    })?;
    if short == new_name {
        return Ok(refname.to_owned());
    }
    branch.rename(new_name, false).map_err(|e| match e.code() {
        ErrorCode::Exists => GitYasError::AlreadyExists(format!("a branch named {new_name:?}")),
        _ => git_error(e),
    })?;
    Ok(format!("refs/heads/{new_name}"))
}

pub fn delete_local_branch(repo: &Repository, refname: &str, force: bool) -> Result<()> {
    let short = refname
        .strip_prefix("refs/heads/")
        .ok_or_else(|| GitYasError::InvalidInput(format!("{refname:?} is not a local branch")))?;
    let mut branch = repo.find_branch(short, BranchType::Local).map_err(|e| match e.code() {
        ErrorCode::NotFound => GitYasError::NotFound(format!("branch {short:?} does not exist")),
        _ => git_error(e),
    })?;
    if branch.is_head() {
        return Err(GitYasError::InvalidInput(format!(
            "cannot delete {short:?}: it is the checked-out branch"
        )));
    }
    if !force && let Some(tip) = branch.get().target() {
        let base = match crate::remote::upstream_refname(repo, refname)?.map(|u| repo.refname_to_id(&u)) {
            Some(Ok(oid)) => Some(oid),
            _ => match repo.head() {
                Ok(h) => h.target(),
                Err(e) if matches!(e.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => None,
                Err(e) => return Err(git_error(e)),
            },
        };
        let merged = match base {
            Some(base) => base == tip || repo.graph_descendant_of(base, tip).map_err(git_error)?,
            None => false,
        };
        if !merged {
            return Err(GitYasError::NotMerged(short.to_string()));
        }
    }
    branch.delete().map_err(git_error)
}

/// Deletes a branch on its remote by remote-tracking ref (`refs/remotes/origin/x`): pushes `:refs/heads/x`,
/// then removes the local remote-tracking ref if libgit2 didn't already.
pub fn delete_remote_branch(repo: &Repository, remote_refname: &str, ctx: &RemoteContext<'_>) -> Result<()> {
    let remote = remote_of_tracking_ref(repo, remote_refname)?
        .ok_or_else(|| GitYasError::InvalidInput(format!("{remote_refname:?} does not belong to a configured remote")))?;
    let name = remote_refname
        .strip_prefix(&format!("refs/remotes/{remote}/"))
        .filter(|n| !n.is_empty() && *n != "HEAD")
        .ok_or_else(|| GitYasError::InvalidInput(format!("{remote_refname:?} is not a remote branch")))?;
    push_refspecs(repo, &remote, &[format!(":refs/heads/{name}")], ctx)?;
    match repo.find_reference(remote_refname) {
        Ok(mut r) => r.delete().map_err(git_error),
        Err(e) if e.code() == ErrorCode::NotFound => Ok(()),
        Err(e) => Err(git_error(e)),
    }
}

/// Publishes local branch `local_refname` as `remote/<remote_branch>` (default: same name) and sets it as the
/// upstream. Fails if that remote branch is already known locally (fetch first) or the remote rejects it.
/// Returns the upstream's short name (`origin/x`).
pub fn create_remote_branch(
    repo: &Repository,
    local_refname: &str,
    remote: &str,
    remote_branch: Option<&str>,
    ctx: &RemoteContext<'_>,
) -> Result<String> {
    let short = local_refname
        .strip_prefix("refs/heads/")
        .ok_or_else(|| GitYasError::InvalidInput(format!("{local_refname:?} is not a local branch")))?;
    let target_name = remote_branch.unwrap_or(short);
    validate_branch_name(target_name)?;
    if !ref_exists(repo, local_refname)? {
        return Err(GitYasError::NotFound(format!(
            "branch {short:?} does not exist or has no commits yet"
        )));
    }
    let tracking = format!("refs/remotes/{remote}/{target_name}");
    if ref_exists(repo, &tracking)? {
        return Err(GitYasError::InvalidInput(format!(
            "branch {remote}/{target_name} already exists on the remote"
        )));
    }
    let merge_ref = format!("refs/heads/{target_name}");
    push_refspecs(repo, remote, &[format!("{local_refname}:{merge_ref}")], ctx)?;
    crate::remote::set_upstream(repo, short, remote, &merge_ref)?;
    Ok(format!("{remote}/{target_name}"))
}
