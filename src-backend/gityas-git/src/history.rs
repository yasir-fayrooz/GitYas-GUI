//! One file's history (`git log --follow -- <path>`) and blame (`git blame`).
//!
//! History walks from HEAD and compares the file's tree entry with each parent's, so only commits that touch the
//! file pay for a diff. A commit that adds the file is checked for a rename (a whole-tree diff with rename
//! detection); from there on the walk follows the old path. Merge commits are listed only when the file differs
//! from every parent (the merge itself changed it, e.g. a conflict resolution).
//!
//! Blame runs libgit2's blame from the newest commit of the target; for the working tree and the index it blames
//! HEAD and then the file's current (or staged) content as a buffer, so uncommitted lines are marked as such.

use std::collections::HashMap;
use std::path::Path;

use git2::{DiffFindOptions, DiffOptions, ErrorCode, Repository, Sort, Tree};
use gityas_core::diff::DiffTarget;
use gityas_core::models::{BlameHunk, Delta, DeltaChange, FileBlame, FileHistoryEntry};
use gityas_core::{GitYasError, Result};

use crate::commits::commit_row;
use crate::convert::{oid_from_git, oid_to_git, repo_path_bytes};
use crate::git_error;
use crate::status::{delta_from_git, ensure_workdir, path_from_bytes};

/// Most commits [`file_history`] looks at (a file untouched for a very long history stops there).
pub const MAX_WALK: usize = 200_000;

/// Commits that changed `path`, newest first, reachable from HEAD; renames are followed. At most `max` entries.
/// An unborn HEAD has no history (empty list).
pub fn file_history(repo: &Repository, path: &Path, max: usize) -> Result<Vec<FileHistoryEntry>> {
    let head = match repo.head() {
        Ok(head) => head.peel_to_commit().map_err(git_error)?,
        Err(err) if matches!(err.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => return Ok(Vec::new()),
        Err(err) => return Err(git_error(err)),
    };
    let mut walk = repo.revwalk().map_err(git_error)?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME).map_err(git_error)?;
    walk.push(head.id()).map_err(git_error)?;

    let mut path = repo_path_bytes(path)?;
    let mut out = Vec::new();
    for oid in walk.take(MAX_WALK) {
        let commit = repo.find_commit(oid.map_err(git_error)?).map_err(git_error)?;
        let tree = commit.tree().map_err(git_error)?;
        let entry = entry_of(&tree, &path);
        let parents = commit
            .parents()
            .map(|p| p.tree())
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(git_error)?;
        let unchanged = if parents.is_empty() {
            entry.is_none()
        } else {
            parents.iter().any(|p| entry_of(p, &path) == entry)
        };
        if unchanged {
            continue;
        }
        let Some(delta) = change_of(repo, parents.first(), &tree, &path)? else {
            continue;
        };
        let renamed_from = delta
            .old_path
            .as_deref()
            .filter(|_| delta.change == DeltaChange::Renamed)
            .map(repo_path_bytes);
        out.push(FileHistoryEntry {
            commit: commit_row(&commit)?,
            file: delta,
        });
        if out.len() >= max {
            break;
        }
        if let Some(old) = renamed_from {
            path = old?;
        }
    }
    Ok(out)
}

/// The file's blob id and mode in `tree`, `None` when it is not there.
fn entry_of(tree: &Tree<'_>, path: &[u8]) -> Option<(git2::Oid, i32)> {
    let path = path_from_bytes(path);
    tree.get_path(&path).ok().map(|e| (e.id(), e.filemode()))
}

/// How the commit with `tree` changed the file at `path` against `parent` (the first parent; `None` for a root
/// commit). An addition is reported as a rename when rename detection finds where it came from.
fn change_of(repo: &Repository, parent: Option<&Tree<'_>>, tree: &Tree<'_>, path: &[u8]) -> Result<Option<Delta>> {
    let mut opts = DiffOptions::new();
    opts.pathspec(path).disable_pathspec_match(true).include_typechange(true);
    let diff = repo.diff_tree_to_tree(parent, Some(tree), Some(&mut opts)).map_err(git_error)?;
    let is_path = |d: &Delta| repo_path_bytes(&d.path).is_ok_and(|p| p == path);
    let Some(delta) = diff.deltas().filter_map(|d| delta_from_git(&d)).find(is_path) else {
        return Ok(None);
    };
    if delta.change != DeltaChange::Added || parent.is_none() {
        return Ok(Some(delta));
    }
    let mut whole = repo.diff_tree_to_tree(parent, Some(tree), None).map_err(git_error)?;
    let mut find = DiffFindOptions::new();
    find.renames(true);
    whole.find_similar(Some(&mut find)).map_err(git_error)?;
    let renamed = whole
        .deltas()
        .filter_map(|d| delta_from_git(&d))
        .find(|d| d.change == DeltaChange::Renamed && is_path(d));
    Ok(Some(renamed.unwrap_or(delta)))
}

/// Blame of `path` as `target` has it: a commit or stash (as of that commit), the index (`staged: true`) or the
/// working tree. Binary files are `InvalidInput`; a path missing from the target is `NotFound`.
pub fn file_blame(repo: &Repository, target: &DiffTarget, path: &Path) -> Result<FileBlame> {
    let want = repo_path_bytes(path)?;
    let blame_path = path_from_bytes(&want);
    let (content, newest) = match target {
        DiffTarget::Commit(oid) | DiffTarget::Stash(oid) => {
            let commit = repo.find_commit(oid_to_git(oid)?).map_err(git_error)?;
            let tree = commit.tree().map_err(git_error)?;
            let entry = tree
                .get_path(&blame_path)
                .map_err(|_| GitYasError::NotFound(format!("{} is not in this commit", path.display())))?;
            let blob = repo.find_blob(entry.id()).map_err(git_error)?;
            (blob.content().to_vec(), Some(commit.id()))
        }
        DiffTarget::WorkingTree { staged: true } => {
            let index = repo.index().map_err(git_error)?;
            let entry = index
                .get_path(&blame_path, 0)
                .ok_or_else(|| GitYasError::NotFound(format!("{} is not staged", path.display())))?;
            let blob = repo.find_blob(entry.id).map_err(git_error)?;
            (blob.content().to_vec(), None)
        }
        DiffTarget::WorkingTree { staged: false } => {
            let workdir = ensure_workdir(repo)?;
            let content = std::fs::read(workdir.join(&blame_path))
                .map_err(|err| GitYasError::NotFound(format!("cannot read {}: {err}", path.display())))?;
            (content, None)
        }
    };
    if content.iter().take(8000).any(|b| *b == 0) {
        return Err(GitYasError::InvalidInput("binary file: there are no lines to blame".into()));
    }
    let (text, lossy) = match String::from_utf8(content) {
        Ok(text) => (text, false),
        Err(err) => (String::from_utf8_lossy(err.as_bytes()).into_owned(), true),
    };
    let line_count = text.lines().count();

    let mut opts = git2::BlameOptions::new();
    if let Some(newest) = newest {
        opts.newest_commit(newest);
    }
    let blame = match repo.blame_file(&blame_path, Some(&mut opts)) {
        Ok(blame) => Some(blame),
        // Not in HEAD yet (a new file): every line is uncommitted.
        Err(err) if newest.is_none() && err.code() == ErrorCode::NotFound => None,
        Err(err) if newest.is_none() && repo.head().is_err() => {
            tracing::debug!("blame on an unborn HEAD: {err}");
            None
        }
        Err(err) => return Err(git_error(err)),
    };
    let Some(base) = blame else {
        let hunks = (line_count > 0).then(|| uncommitted(path, 0, line_count)).into_iter().collect();
        return Ok(FileBlame { text, lossy, hunks });
    };
    let buffered;
    let blame = if newest.is_some() {
        &base
    } else {
        buffered = base.blame_buffer(text.as_bytes()).map_err(git_error)?;
        &buffered
    };

    // Author and summary per commit, read from the commit: a buffer blame's hunks carry no signature.
    let mut commits: HashMap<git2::Oid, (String, i64, String)> = HashMap::new();
    let mut hunks = Vec::with_capacity(blame.len());
    for hunk in blame.iter() {
        let start = hunk.final_start_line().saturating_sub(1);
        let lines = hunk.lines_in_hunk();
        if lines == 0 || start >= line_count {
            continue;
        }
        let lines = lines.min(line_count - start);
        let id = hunk.final_commit_id();
        if id.is_zero() {
            hunks.push(uncommitted(path, start, lines));
            continue;
        }
        let (author_name, author_time, summary) = match commits.get(&id) {
            Some(info) => info.clone(),
            None => {
                let commit = repo.find_commit(id).map_err(git_error)?;
                let author = commit.author();
                let info = (
                    String::from_utf8_lossy(author.name_bytes()).into_owned(),
                    author.when().seconds(),
                    commit
                        .summary_bytes()
                        .map(|s| String::from_utf8_lossy(s).into_owned())
                        .unwrap_or_default(),
                );
                commits.insert(id, info.clone());
                info
            }
        };
        hunks.push(BlameHunk {
            commit: Some(oid_from_git(id)?),
            author_name,
            author_time,
            summary,
            path: hunk.path().map_or_else(|| path.to_path_buf(), Path::to_path_buf),
            start,
            lines,
        });
    }
    hunks.sort_by_key(|h| h.start);
    Ok(FileBlame { text, lossy, hunks })
}

fn uncommitted(path: &Path, start: usize, lines: usize) -> BlameHunk {
    BlameHunk {
        commit: None,
        author_name: "Not committed yet".into(),
        author_time: 0,
        summary: String::new(),
        path: path.to_path_buf(),
        start,
        lines,
    }
}
