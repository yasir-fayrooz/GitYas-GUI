//! Live history walk (`get_commits_live`, kept permanently as cache oracle/fallback), sorting, head,
//! commit staged files, commit file lists, commit bodies.
//!
//! Display order ([`sort_commits`]) reproduces the old `commit.rs::sort_commits` (sort by committer time, newest
//! first, then a depth-first pass over *children* that emits a commit only after all of its children), so children
//! always precede parents even with clock skew. Differences from the old code, all deliberate:
//! * ties on committer time break by object id (the old order depended on revwalk order, i.e. was unspecified);
//! * the DFS is iterative (the old recursive DFS could overflow the stack on long histories);
//! * parents outside the list (shallow boundary, hidden commits) are ignored instead of `unwrap()`-panicking.

use std::collections::{HashMap, HashSet};

use git2::{ErrorCode, Repository, RepositoryState};
use gityas_core::models::{Commit, CommitSignature, Delta, Head, RefTip, ResetMode};
use gityas_core::{GitYasError, Oid, Result};

use crate::cache::ref_tips;
use crate::convert::{oid_from_git, oid_to_git};
use crate::git_error;
use crate::gitoxide::{self, GixRepo};
use crate::status::{delta_from_git, sort_deltas};

/// Converts one commit into the listed/cached row. Author name/email and summary are decoded lossily for display
/// and `lossy` is set when any of them was not valid UTF-8 ([`commit_body`] reports the full message).
pub fn commit_row(commit: &git2::Commit<'_>) -> Result<Commit> {
    let author = commit.author();
    let mut lossy = false;
    let mut decode = |bytes: &[u8]| match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => {
            lossy = true;
            String::from_utf8_lossy(bytes).into_owned()
        }
    };
    let author_name = decode(author.name_bytes());
    let author_email = decode(author.email_bytes());
    let summary = commit.summary_bytes().map(&mut decode).unwrap_or_default();
    let description = commit
        .body_bytes()
        .map(|body| Commit::description_preview(&String::from_utf8_lossy(body)))
        .unwrap_or_default();
    Ok(Commit {
        id: oid_from_git(commit.id())?,
        parents: commit.parent_ids().map(oid_from_git).collect::<Result<Vec<_>>>()?,
        author_name,
        author_email,
        author_time: author.when().seconds(),
        committer_time: commit.time().seconds(),
        summary,
        description,
        lossy,
    })
}

/// Full live walk of everything reachable from [`ref_tips`] (local branches, remote-tracking branches, tags peeled
/// to commits, and `HEAD`, so a detached HEAD's commits are shown; stashes and notes are not part of the history),
/// in display order. This is the oracle and fallback for the commit-graph cache.
///
/// gitoxide: the walk reads parents from the commit-graph file when there is one, then the rows are decoded in
/// parallel ([`crate::cache::reach_rows`]).
pub fn get_commits_live<R: GixRepo + ?Sized>(repo: &R) -> Result<Vec<Commit>> {
    let repo = repo.gix()?;
    get_commits_live_from(&repo, &ref_tips(&*repo)?)
}

/// [`get_commits_live`] from tips already read (the engine reads refs once per refresh).
pub fn get_commits_live_from(repo: &gix::Repository, tips: &[RefTip]) -> Result<Vec<Commit>> {
    let tip_ids = tips.iter().map(|tip| gitoxide::oid_to_gix(&tip.oid)).collect::<Result<Vec<_>>>()?;
    Ok(sort_commits(crate::cache::reach_rows(repo, &tip_ids, &[])?))
}

/// The newest `max` commits reachable from [`ref_tips`], in display order, or `None` when the history has no more
/// than `max` commits (the full history is then no more work). A first page to show while the full history loads;
/// with clock skew its order can differ slightly from [`get_commits_live`]'s.
///
/// gitoxide's commit-time ordering is incremental (a priority queue over the frontier, commit times from the
/// commit-graph when present), so this reads about `max` commits, not the whole history.
pub fn get_commits_newest<R: GixRepo + ?Sized>(repo: &R, max: usize) -> Result<Option<Vec<Commit>>> {
    let repo = repo.gix()?;
    get_commits_newest_from(&repo, &ref_tips(&*repo)?, max)
}

/// [`get_commits_newest`] from tips already read.
pub fn get_commits_newest_from(repo: &gix::Repository, tips: &[RefTip], max: usize) -> Result<Option<Vec<Commit>>> {
    let mut tip_ids = tips.iter().map(|tip| gitoxide::oid_to_gix(&tip.oid)).collect::<Result<Vec<_>>>()?;
    tip_ids.sort_unstable();
    tip_ids.dedup();
    let walk = repo
        .rev_walk(tip_ids)
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            gix::revision::walk::CommitTimeOrder::NewestFirst,
        ))
        .all()
        .map_err(gitoxide::gix_error)?;
    let mut ids = Vec::with_capacity(max);
    for info in walk {
        let info = info.map_err(gitoxide::gix_error)?;
        if ids.len() == max {
            return Ok(Some(sort_commits(gitoxide::decode_rows(repo, &ids)?)));
        }
        ids.push(info.id);
    }
    Ok(None)
}

/// Sorts commits into display order (see the module docs). Pure; the cache must use this same function so
/// cached and live results are identical. Duplicate ids keep their first occurrence.
pub fn sort_commits(commits: Vec<Commit>) -> Vec<Commit> {
    let mut seen_ids = HashSet::with_capacity(commits.len());
    let commits: Vec<Commit> = commits.into_iter().filter(|c| seen_ids.insert(c.id.clone())).collect();
    let n = commits.len();

    // Indexing below is only with indices in `0..n` produced here (never from git or user data), so it cannot panic.
    // `order`: indices sorted newest first (ties by id); `rank[i]`: position of commit `i` in `order`.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        let (ca, cb) = (&commits[a], &commits[b]);
        cb.committer_time.cmp(&ca.committer_time).then_with(|| ca.id.cmp(&cb.id))
    });
    let mut rank = vec![0usize; n];
    for (pos, &idx) in order.iter().enumerate() {
        rank[idx] = pos;
    }

    let index_of: HashMap<&Oid, usize> = commits.iter().enumerate().map(|(i, c)| (&c.id, i)).collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (child, commit) in commits.iter().enumerate() {
        for parent in &commit.parents {
            if let Some(&p) = index_of.get(parent) {
                children[p].push(child);
            }
        }
    }
    for list in &mut children {
        list.sort_by_key(|&c| rank[c]);
        list.dedup();
    }

    // Iterative post-order DFS over children: a commit is emitted after all of its descendants.
    let mut seen = vec![false; n];
    let mut out = Vec::with_capacity(n);
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for &start in &order {
        if seen[start] {
            continue;
        }
        seen[start] = true;
        stack.push((start, 0));
        while let Some((node, next)) = stack.last_mut() {
            let node = *node;
            if let Some(&child) = children[node].get(*next) {
                *next += 1;
                if !seen[child] {
                    seen[child] = true;
                    stack.push((child, 0));
                }
            } else {
                out.push(node);
                stack.pop();
            }
        }
    }

    let mut slots: Vec<Option<Commit>> = commits.into_iter().map(Some).collect();
    out.into_iter().filter_map(|i| slots.get_mut(i).and_then(Option::take)).collect()
}

/// Keeps only commits reachable from `tips` through parent links inside `commits`, preserving input order. This is
/// the old "unreachable removal" (`remove_unreachable_commits`) as a pure function, for the cache's reachability
/// step. Tips not present in `commits` are ignored.
pub fn retain_reachable(commits: Vec<Commit>, tips: &[Oid]) -> Vec<Commit> {
    let by_id: HashMap<&Oid, &Commit> = commits.iter().map(|c| (&c.id, c)).collect();
    let mut reachable: HashSet<Oid> = HashSet::new();
    let mut frontier: Vec<&Oid> = tips.iter().filter(|t| by_id.contains_key(t)).collect();
    while let Some(id) = frontier.pop() {
        if !reachable.insert(id.clone()) {
            continue;
        }
        if let Some(commit) = by_id.get(id) {
            frontier.extend(commit.parents.iter().filter(|p| by_id.contains_key(p) && !reachable.contains(*p)));
        }
    }
    drop(by_id);
    commits.into_iter().filter(|c| reachable.contains(&c.id)).collect()
}

/// Where HEAD points: a branch, a detached commit, or an unborn branch (fresh repository).
pub fn get_head<R: GixRepo + ?Sized>(repo: &R) -> Result<Head> {
    let repo = repo.gix()?;
    let head = repo.head().map_err(gitoxide::gix_error)?;
    let referent = || {
        head.referent_name()
            .ok_or_else(|| GitYasError::Git("HEAD is neither a branch nor a commit".into()))
            .and_then(|name| utf8_ref_name(name.as_bstr()))
    };
    if head.is_unborn() {
        let refname = referent()?;
        return Ok(Head::Unborn {
            name: short_branch_name(&refname),
            refname,
        });
    }
    let target = head
        .id()
        .ok_or_else(|| GitYasError::Git("HEAD does not resolve to an object id".into()))
        .and_then(|id| gitoxide::oid_from_gix(&id))?;
    if head.is_detached() {
        return Ok(Head::Detached { target });
    }
    let refname = referent()?;
    Ok(Head::Branch {
        name: short_branch_name(&refname),
        refname,
        target,
    })
}

fn utf8_ref_name(bytes: &[u8]) -> Result<String> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| GitYasError::InvalidInput(format!("reference name is not valid UTF-8: {}", String::from_utf8_lossy(bytes))))
}

fn short_branch_name(refname: &str) -> String {
    refname.strip_prefix("refs/heads/").unwrap_or(refname).to_owned()
}

/// Builds a libgit2 signature, rejecting an empty or invalid identity with a clear error instead of panicking.
/// Shared by commit and stash creation.
pub fn signature_now(signature: &CommitSignature) -> Result<git2::Signature<'static>> {
    if !signature.is_valid() {
        return Err(GitYasError::SignatureRequired(
            "set your commit name and email (commit signature) before committing".into(),
        ));
    }
    git2::Signature::now(signature.name.trim(), signature.email.trim())
        .map_err(|e| GitYasError::SignatureRequired(format!("invalid commit signature: {}", e.message())))
}

/// Joins subject and optional description and normalises with `git2::message_prettify` (no comment stripping):
/// trailing whitespace and surplus blank lines removed, exactly one trailing newline. An empty
/// subject is rejected.
pub fn build_commit_message(message: &str, description: Option<&str>) -> Result<String> {
    let subject = message.trim();
    if subject.is_empty() {
        return Err(GitYasError::InvalidInput("commit message is empty".into()));
    }
    let raw = match description.map(str::trim).filter(|d| !d.is_empty()) {
        Some(body) => format!("{subject}\n\n{body}"),
        None => subject.to_owned(),
    };
    git2::message_prettify(raw, None).map_err(git_error)
}

/// Commits the index on top of HEAD and returns the new commit id.
///
/// * Works on an unborn HEAD (first commit, no parents).
/// * Rejects an empty/invalid signature and an empty message; the message is prettified.
/// * Rejects an index with conflicts, and "nothing staged" (tree identical to HEAD's) except while a merge is in
///   progress (a merge commit may legitimately have no changes, e.g. after resolving to "ours").
/// * Merge in progress: parents are HEAD followed by every `MERGE_HEAD`; afterwards the merge state is cleaned up.
///   Cherry-pick / revert in progress: a normal commit, then the state is cleaned up (as `git commit` does).
///   Rebase, `git am` and multi-commit sequences are refused (committing would corrupt their state).
///   Bisect is allowed and its state is kept.
pub fn commit_staged_files(repo: &mut Repository, signature: &CommitSignature, message: &str, description: Option<&str>) -> Result<Oid> {
    let sig = signature_now(signature)?;
    let message = build_commit_message(message, description)?;

    let state = repo.state();
    let cleanup = match state {
        RepositoryState::Clean | RepositoryState::Bisect => false,
        RepositoryState::Merge | RepositoryState::Revert | RepositoryState::CherryPick => true,
        other => {
            return Err(GitYasError::InvalidInput(format!(
                "cannot commit while an operation is in progress ({other:?}); finish or abort it first"
            )));
        }
    };

    let mut merge_heads = Vec::new();
    if state == RepositoryState::Merge {
        repo.mergehead_foreach(|oid| {
            merge_heads.push(*oid);
            true
        })
        .map_err(git_error)?;
    }

    let mut index = repo.index().map_err(git_error)?;
    index.read(false).map_err(git_error)?;
    if index.has_conflicts() {
        return Err(GitYasError::InvalidInput("resolve all conflicts before committing".into()));
    }
    let tree_id = index.write_tree().map_err(git_error)?;
    let tree = repo.find_tree(tree_id).map_err(git_error)?;

    let head = head_commit(repo)?;
    if merge_heads.is_empty() {
        let unchanged = match &head {
            Some(head) => head.tree_id() == tree_id,
            None => tree.is_empty(),
        };
        if unchanged {
            return Err(GitYasError::InvalidInput("nothing staged to commit".into()));
        }
    }

    let mut parents: Vec<git2::Commit<'_>> = head.into_iter().collect();
    for oid in merge_heads {
        parents.push(repo.find_commit(oid).map_err(git_error)?);
    }
    let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
    let oid = repo
        .commit(Some("HEAD"), &sig, &sig, &message, &tree, &parent_refs)
        .map_err(git_error)?;
    if cleanup {
        repo.cleanup_state().map_err(git_error)?;
    }
    oid_from_git(oid)
}

/// Replaces the message of `commit`, which must be HEAD or one of its ancestors, and returns the reworded commit's
/// new id. Every commit between it and HEAD is rewritten onto the new id (trees, authors and committers are kept,
/// so the index and working tree are untouched; signatures such as `gpgsig` are dropped, as `git rebase` does).
/// HEAD's branch (or the detached HEAD) is moved to the new tip; other refs pointing into the rewritten range are
/// left where they are.
///
/// * The message is prettified like a new commit's; an empty one is rejected. An unchanged message is a no-op.
/// * Refused while an operation (merge, rebase, …) is in progress, and when a rewritten commit's message is not
///   valid UTF-8 (it could not be written back unchanged).
pub fn reword_commit(repo: &Repository, commit: &Oid, message: &str) -> Result<Oid> {
    if message.trim().is_empty() {
        return Err(GitYasError::InvalidInput("commit message is empty".into()));
    }
    let message = git2::message_prettify(message.trim(), None).map_err(git_error)?;
    if repo.state() != RepositoryState::Clean {
        return Err(GitYasError::InvalidInput(format!(
            "cannot edit a commit message while an operation is in progress ({:?}); finish or abort it first",
            repo.state()
        )));
    }
    let target = repo.find_commit(oid_to_git(commit)?).map_err(git_error)?;
    let head = head_commit(repo)?.ok_or_else(|| GitYasError::InvalidInput("HEAD has no commits".into()))?;
    if head.id() != target.id()
        && !gitoxide::is_ancestor(
            &*repo.gix()?,
            gitoxide::oid_from_git2(target.id())?,
            gitoxide::oid_from_git2(head.id())?,
        )?
    {
        return Err(GitYasError::InvalidInput(
            "only commits on the checked-out branch (HEAD and its ancestors) can be edited".into(),
        ));
    }
    if target.message_raw_bytes() == message.as_bytes() {
        return oid_from_git(target.id());
    }

    let tree = target.tree().map_err(git_error)?;
    let parents: Vec<git2::Commit<'_>> = target.parents().collect();
    let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
    let reworded = repo
        .commit(None, &target.author(), &target.committer(), &message, &tree, &parent_refs)
        .map_err(git_error)?;

    // Commits reachable from HEAD but not from the target, parents first; only those with a rewritten parent change.
    let mut rewritten = HashMap::from([(target.id(), reworded)]);
    let mut walk = repo.revwalk().map_err(git_error)?;
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE).map_err(git_error)?;
    walk.push(head.id()).map_err(git_error)?;
    walk.hide(target.id()).map_err(git_error)?;
    for oid in walk {
        let old = repo.find_commit(oid.map_err(git_error)?).map_err(git_error)?;
        if !old.parent_ids().any(|p| rewritten.contains_key(&p)) {
            continue;
        }
        let text = old.message_raw().ok_or_else(|| {
            GitYasError::InvalidInput(format!(
                "commit {} has a message that is not valid UTF-8 and cannot be rewritten",
                old.id()
            ))
        })?;
        let parents = old
            .parent_ids()
            .map(|p| repo.find_commit(rewritten.get(&p).copied().unwrap_or(p)))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(git_error)?;
        let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
        let tree = old.tree().map_err(git_error)?;
        let new = repo
            .commit(None, &old.author(), &old.committer(), text, &tree, &parent_refs)
            .map_err(git_error)?;
        rewritten.insert(old.id(), new);
    }

    let new_head = rewritten.get(&head.id()).copied().unwrap_or(head.id());
    let summary = message.lines().next().unwrap_or_default();
    let log = format!("reword: {summary}");
    if repo.head_detached().map_err(git_error)? {
        repo.set_head_detached(new_head).map_err(git_error)?;
    } else {
        let mut head_ref = repo.head().map_err(git_error)?.resolve().map_err(git_error)?;
        head_ref.set_target(new_head, &log).map_err(git_error)?;
    }
    oid_from_git(reworded)
}

/// Moves HEAD's branch (or the detached HEAD) to `commit`, like `git reset --soft` / `--hard`.
///
/// * Soft keeps the index and working tree, so everything since `commit` shows up as staged changes. Refused while
///   an operation (merge, rebase, …) is in progress, as git does.
/// * Hard resets the index and working tree to `commit` (untracked files are left alone) and clears any
///   in-progress merge state.
pub fn reset_to_commit(repo: &Repository, commit: &Oid, mode: ResetMode) -> Result<()> {
    let target = repo.find_commit(oid_to_git(commit)?).map_err(|e| match e.code() {
        ErrorCode::NotFound => GitYasError::NotFound(format!("commit {commit} does not exist")),
        _ => git_error(e),
    })?;
    if head_commit(repo)?.is_none() {
        return Err(GitYasError::InvalidInput("HEAD has no commits".into()));
    }
    let kind = match mode {
        ResetMode::Soft => {
            if repo.state() != RepositoryState::Clean {
                return Err(GitYasError::InvalidInput(format!(
                    "cannot soft reset while an operation is in progress ({:?}); finish or abort it first",
                    repo.state()
                )));
            }
            git2::ResetType::Soft
        }
        ResetMode::Hard => git2::ResetType::Hard,
    };
    repo.reset(target.as_object(), kind, None).map_err(git_error)?;
    if mode == ResetMode::Hard {
        repo.cleanup_state().map_err(git_error)?;
    }
    Ok(())
}

/// HEAD's commit, `None` on an unborn branch.
pub(crate) fn head_commit(repo: &Repository) -> Result<Option<git2::Commit<'_>>> {
    match repo.head() {
        Ok(head) => head.peel_to_commit().map(Some).map_err(git_error),
        Err(err) if err.code() == ErrorCode::UnbornBranch || err.code() == ErrorCode::NotFound => Ok(None),
        Err(err) => Err(git_error(err)),
    }
}

/// Files changed by a commit relative to its **first parent** (merge commits show what the merge brought into the
/// mainline); root commits are compared with the empty tree. Renames are detected; a file↔symlink change is one
/// `Typechange` entry (not delete + add). Sorted by path.
pub fn get_commit_files_list(repo: &Repository, commit: &Oid) -> Result<Vec<Delta>> {
    let commit = repo.find_commit(oid_to_git(commit)?).map_err(git_error)?;
    let tree = commit.tree().map_err(git_error)?;
    let parent_tree = match commit.parent_ids().next() {
        Some(parent) => Some(repo.find_commit(parent).and_then(|p| p.tree()).map_err(git_error)?),
        None => None,
    };
    let mut opts = git2::DiffOptions::new();
    opts.include_typechange(true);
    let mut diff = repo
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))
        .map_err(git_error)?;
    let mut find = git2::DiffFindOptions::new();
    find.renames(true);
    diff.find_similar(Some(&mut find)).map_err(git_error)?;
    let mut files: Vec<Delta> = diff.deltas().filter_map(|d| delta_from_git(&d)).collect();
    sort_deltas(&mut files);
    Ok(files)
}

pub use gityas_core::models::CommitBody;

/// Full message of one commit, flagging non-UTF-8 content instead of silently replacing it.
pub fn commit_body(repo: &Repository, commit: &Oid) -> Result<CommitBody> {
    let commit = repo.find_commit(oid_to_git(commit)?).map_err(git_error)?;
    let bytes = commit.message_raw_bytes();
    let (message, lossy) = match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_owned(), false),
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), true),
    };
    Ok(CommitBody {
        message,
        lossy,
        encoding: commit.message_encoding().map(str::to_owned),
    })
}
