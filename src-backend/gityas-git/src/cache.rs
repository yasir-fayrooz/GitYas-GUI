//! WP5: commit-graph cache primitives (§3.4): `ref_tips`, `fingerprint`, `walk_new_commits` (push new / hide old tips),
//! `walk_until_known` (stop at already-known commits; WP8e's in-memory incremental path).
//!
//! The engine (WP8e) glues these to the cache DAO: `ref_tips` → `fingerprint` → equal to the stored one? serve from
//! cache : `walk_new_commits(new, stored_old)` → insert → store `new` as the snapshot. The live walk
//! (`commits::get_commits_live`) uses the same [`ref_tips`], so cached and live views see the same tip set.

use std::collections::HashSet;

use git2::{ErrorCode, Repository};
use gityas_core::models::{CommitRow, RefTip, RefsFingerprint};
use gityas_core::{Oid, Result};

use crate::commits::commit_row;
use crate::convert::{StableHasher, oid_from_git, oid_to_git};
use crate::git_error;

/// Ref namespaces that contribute history. Excluded on purpose: `refs/stash` (stashes are listed separately and
/// their index/untracked helper commits would clutter the graph; same as the old code), `refs/notes/*` (notes
/// commits are not history), `refs/replace/*` (see [`should_bypass_cache`]), and anything else (`refs/pull/*`,
/// `refs/original/*`, bisect refs).
const TIP_NAMESPACES: [&str; 3] = ["refs/heads/", "refs/remotes/", "refs/tags/"];

/// Pseudo-refname used for HEAD, so a detached HEAD's commits are reachable (the old code omitted HEAD and lost
/// them, bug-log WP5.4).
pub const HEAD_TIP: &str = "HEAD";

/// All live ref tips peeled to commits, sorted by `(refname, oid)`. Annotated tags are peeled to their commit;
/// refs that do not peel to a commit (tags of trees/blobs, dangling refs) are skipped with a warning instead of
/// failing the whole history (the old `revwalk.push_ref` errored on them). Non-UTF-8 ref names are skipped with a
/// warning (they cannot be represented in `RefTip`). `HEAD` is included (as [`HEAD_TIP`]) unless unborn.
pub fn ref_tips(repo: &Repository) -> Result<Vec<RefTip>> {
    let mut tips = Vec::new();
    for reference in repo.references().map_err(git_error)? {
        let reference = reference.map_err(git_error)?;
        let Ok(name) = std::str::from_utf8(reference.name_bytes()) else {
            tracing::warn!(name = %String::from_utf8_lossy(reference.name_bytes()), "skipping non-UTF-8 ref name");
            continue;
        };
        if !TIP_NAMESPACES.iter().any(|ns| name.starts_with(ns)) {
            continue;
        }
        match reference.peel_to_commit() {
            Ok(commit) => tips.push(RefTip {
                refname: name.to_owned(),
                oid: oid_from_git(commit.id())?,
            }),
            Err(err) => tracing::warn!(refname = name, error = err.message(), "ref does not peel to a commit; skipped"),
        }
    }
    match repo.head() {
        Ok(head) => match head.peel_to_commit() {
            Ok(commit) => tips.push(RefTip {
                refname: HEAD_TIP.to_owned(),
                oid: oid_from_git(commit.id())?,
            }),
            Err(err) => tracing::warn!(error = err.message(), "HEAD does not peel to a commit; skipped"),
        },
        Err(err) if err.code() == ErrorCode::UnbornBranch || err.code() == ErrorCode::NotFound => {}
        Err(err) => return Err(git_error(err)),
    }
    tips.sort();
    tips.dedup();
    Ok(tips)
}

/// Format version of [`fingerprint`]; bump if the encoding changes (old fingerprints then never match).
const FINGERPRINT_VERSION: u8 = 1;

/// Stable fingerprint of a tip set: `[version][tip count, u64 LE][FNV-1a 64 of the sorted, length-prefixed
/// (refname, oid) pairs, u64 LE]`. Order-independent (input is sorted first) and identical across runs, processes
/// and platforms. A 64-bit non-cryptographic hash is enough: it is only ever compared with the previous
/// fingerprint of the same repository, so an accidental collision (≈2⁻⁶⁴) is the only failure mode, and forging
/// one needs write access to the repository's refs.
pub fn fingerprint(tips: &[RefTip]) -> RefsFingerprint {
    let mut sorted: Vec<&RefTip> = tips.iter().collect();
    sorted.sort();
    sorted.dedup();
    let mut h = StableHasher::default();
    for tip in &sorted {
        h.write(&(tip.refname.len() as u64).to_le_bytes());
        h.write(tip.refname.as_bytes());
        h.write(&(tip.oid.as_bytes().len() as u64).to_le_bytes());
        h.write(tip.oid.as_bytes());
    }
    let mut bytes = Vec::with_capacity(17);
    bytes.push(FINGERPRINT_VERSION);
    bytes.extend_from_slice(&(sorted.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&h.finish().to_le_bytes());
    RefsFingerprint(bytes)
}

/// Commits reachable from `new_tips` but not from `old_tips` (`revwalk.push(new)` + `revwalk.hide(old)`), in
/// unspecified order (the engine sorts with `commits::sort_commits`).
///
/// Correctness: the result is `reach(new) \ reach(old)`; if the cache already holds `reach(old)` (the engine's
/// invariant: `old_tips` is the snapshot stored together with its walked commits), the union is `reach(new)`,
/// even after rebase / force-push / branch delete. Old tips whose objects no longer exist (gc'd after a rebase or
/// branch delete) or no longer peel to a commit are not hidden, which can only make the result *larger* (already
/// cached commits are re-returned; inserts are idempotent), never miss a commit. If hiding fails mid-walk (e.g. an
/// ancestor of an old tip was pruned), the walk is retried without hiding (a full walk from `new_tips`).
pub fn walk_new_commits(repo: &Repository, new_tips: &[RefTip], old_tips: &[RefTip]) -> Result<Vec<CommitRow>> {
    match walk(repo, new_tips, old_tips) {
        Ok(rows) => Ok(rows),
        Err(err) if !old_tips.is_empty() => {
            tracing::warn!(error = %err, "incremental walk failed while hiding old tips; retrying as a full walk");
            walk(repo, new_tips, &[])
        }
        Err(err) => Err(err),
    }
}

/// `reach(tips) \ known`, unordered. `known` must be closed under parents (e.g. a complete previously loaded
/// history); the walk stops at known commits instead of hiding old tips. Much cheaper than [`walk_new_commits`] for a
/// small change on a large history: libgit2's `hide` marks every ancestor of the hidden tips (WP8e measured ~70 ms on
/// 100k commits for a one-commit change vs ~0 ms here).
pub fn walk_until_known(repo: &Repository, tips: &[RefTip], known: &HashSet<&Oid>) -> Result<Vec<CommitRow>> {
    let mut seen: HashSet<Oid> = HashSet::new();
    let mut stack: Vec<Oid> = Vec::new();
    for tip in tips {
        if !known.contains(&tip.oid) && seen.insert(tip.oid.clone()) {
            stack.push(tip.oid.clone());
        }
    }
    let mut rows = Vec::new();
    while let Some(oid) = stack.pop() {
        let commit = repo.find_commit(oid_to_git(&oid)?).map_err(git_error)?;
        let row = commit_row(&commit)?;
        for parent in &row.parents {
            if !known.contains(parent) && seen.insert(parent.clone()) {
                stack.push(parent.clone());
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

fn walk(repo: &Repository, new_tips: &[RefTip], old_tips: &[RefTip]) -> Result<Vec<CommitRow>> {
    let mut revwalk = repo.revwalk().map_err(git_error)?;
    let mut pushed = HashSet::new();
    for tip in new_tips {
        if pushed.insert(&tip.oid) {
            revwalk.push(oid_to_git(&tip.oid)?).map_err(git_error)?;
        }
    }
    let mut hidden = HashSet::new();
    for tip in old_tips {
        if !hidden.insert(&tip.oid) {
            continue;
        }
        let oid = oid_to_git(&tip.oid)?;
        let commit = match repo.find_object(oid, None) {
            Ok(object) => match object.peel_to_commit() {
                Ok(commit) => commit,
                Err(_) => continue, // no longer a commit-ish: nothing to hide
            },
            Err(err) if err.code() == ErrorCode::NotFound => continue, // gc'd: nothing to hide
            Err(err) => return Err(git_error(err)),
        };
        revwalk.hide(commit.id()).map_err(git_error)?;
    }
    let mut rows = Vec::new();
    for oid in revwalk {
        let oid = oid.map_err(git_error)?;
        rows.push(commit_row(&repo.find_commit(oid).map_err(git_error)?)?);
    }
    Ok(rows)
}

/// `true` when the cache must not be used and the engine should do a live walk: shallow clones (history
/// boundaries move on deepen/unshallow without any ref change), grafts (`info/grafts`), and replace refs
/// (`refs/replace/*`, which rewrite history without changing tips; libgit2 does not apply them, so a cache keyed on
/// tips could disagree with the git CLI).
pub fn should_bypass_cache(repo: &Repository) -> Result<bool> {
    if repo.is_shallow() {
        return Ok(true);
    }
    match std::fs::metadata(repo.commondir().join("info").join("grafts")) {
        Ok(_) => return Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    let mut replace = repo.references_glob("refs/replace/*").map_err(git_error)?;
    Ok(replace.next().is_some())
}
