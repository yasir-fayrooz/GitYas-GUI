//! Commit-graph cache primitives: `ref_tips`, `fingerprint`, `walk_new_commits` (push new / hide old tips),
//! `walk_until_known` (stop at already-known commits; the engine's in-memory incremental path).
//!
//! The engine glues these to the cache DAO: `ref_tips` → `fingerprint` → equal to the stored one? serve from
//! cache : `walk_new_commits(new, stored_old)` → insert → store `new` as the snapshot. The live walk
//! (`commits::get_commits_live`) uses the same [`ref_tips`], so cached and live views see the same tip set.
//!
//! Everything here reads through gitoxide ([`crate::gitoxide`]).

use std::collections::HashSet;

use gityas_core::models::{CommitRow, RefTip, RefsFingerprint};
use gityas_core::{Oid, Result};
use gix::ObjectId;

use crate::convert::StableHasher;
use crate::gitoxide::{self, GixRepo, gix_error, oid_to_gix};

/// Ref namespaces that contribute history. Excluded on purpose: `refs/stash` (stashes are listed separately and
/// their index/untracked helper commits would clutter the graph; same as the old code), `refs/notes/*` (notes
/// commits are not history), `refs/replace/*` (see [`should_bypass_cache`]), and anything else (`refs/pull/*`,
/// `refs/original/*`, bisect refs).
pub use crate::refscan::TIP_NAMESPACES;

/// Pseudo-refname used for HEAD, so a detached HEAD's commits are reachable (the old code omitted HEAD and lost
/// them).
pub const HEAD_TIP: &str = "HEAD";

/// All live ref tips peeled to commits, sorted by `(refname, oid)`. Annotated tags are peeled to their commit;
/// refs that do not peel to a commit (tags of trees/blobs, dangling refs) are skipped with a warning instead of
/// failing the whole history (the old `revwalk.push_ref` errored on them). Non-UTF-8 ref names are skipped with a
/// warning (they cannot be represented in `RefTip`). `HEAD` is included (as [`HEAD_TIP`]) unless unborn.
pub fn ref_tips<R: GixRepo + ?Sized>(repo: &R) -> Result<Vec<RefTip>> {
    let (snapshot, repo) = crate::refscan::scan_refs_once(repo)?;
    snapshot.tips(crate::refscan::head_commit(&repo)?)
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
pub fn walk_new_commits<R: GixRepo + ?Sized>(repo: &R, new_tips: &[RefTip], old_tips: &[RefTip]) -> Result<Vec<CommitRow>> {
    let repo = repo.gix()?;
    match walk(&repo, new_tips, old_tips) {
        Ok(rows) => Ok(rows),
        Err(err) if !old_tips.is_empty() => {
            tracing::warn!(error = %err, "incremental walk failed while hiding old tips; retrying as a full walk");
            walk(&repo, new_tips, &[])
        }
        Err(err) => Err(err),
    }
}

/// `reach(tips) \ known`, unordered. `known` must be closed under parents (e.g. a complete previously loaded
/// history); the walk stops at known commits instead of hiding old tips. Much cheaper than [`walk_new_commits`] for a
/// small change on a large history: hiding marks every ancestor of the hidden tips that the walk could reach.
pub fn walk_until_known<R: GixRepo + ?Sized>(repo: &R, tips: &[RefTip], known: &HashSet<&Oid>) -> Result<Vec<CommitRow>> {
    let repo = repo.gix()?;
    let mut starts = Vec::new();
    for tip in tips {
        if !known.contains(&tip.oid) {
            starts.push(oid_to_gix(&tip.oid)?);
        }
    }
    depth_first_rows(&repo, &starts, |oid| known.contains(oid))
}

/// Every commit reachable from `tips` but not from `hidden`, as rows, in **unspecified order**.
///
/// * With a commit-graph file (and nothing hidden): the walk reads ids and parents from the graph without touching
///   the object database, then [`gitoxide::decode_rows`] decodes the rows in parallel.
/// * Without one: a single depth-first pass that reads each commit once (row and parents from the same decode);
///   a separate walk would decompress every commit twice.
/// * With `hidden` tips, a shallow repository or replace refs: gitoxide's walk (hiding, shallow boundaries as
///   roots, replacements), then decoding.
pub fn reach_rows(repo: &gix::Repository, tips: &[ObjectId], hidden: &[ObjectId]) -> Result<Vec<CommitRow>> {
    let mut tips = tips.to_vec();
    tips.sort_unstable();
    tips.dedup();
    if tips.is_empty() {
        return Ok(Vec::new());
    }
    let graph = if hidden.is_empty() {
        repo.commit_graph_if_enabled().unwrap_or_else(|err| {
            tracing::debug!(error = %format!("{err:#}"), "commit-graph unusable; walking the object database");
            None
        })
    } else {
        None
    };
    let plain = hidden.is_empty() && graph.is_none() && !repo.is_shallow().map_err(gix_error)? && !has_replace_refs(repo)?;
    if plain {
        return depth_first_rows(repo, &tips, |_| false);
    }
    let walk = repo
        .rev_walk(tips)
        .with_hidden(hidden.iter().copied())
        .with_commit_graph(graph)
        .all()
        .map_err(gix_error)?;
    let ids = walk
        .map(|info| info.map(|info| info.id))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(gix_error)?;
    gitoxide::decode_rows(repo, &ids)
}

/// Depth-first over parents from `starts`, decoding each commit once; parents for which `stop` is true are not
/// entered. Every commit must exist (not for shallow repositories).
fn depth_first_rows(repo: &gix::Repository, starts: &[ObjectId], stop: impl Fn(&Oid) -> bool) -> Result<Vec<CommitRow>> {
    let mut seen: gix::hashtable::HashSet<ObjectId> = gix::hashtable::HashSet::default();
    let mut stack: Vec<ObjectId> = Vec::new();
    for id in starts {
        if seen.insert(*id) {
            stack.push(*id);
        }
    }
    let mut rows = Vec::new();
    let mut buf = Vec::new();
    while let Some(id) = stack.pop() {
        let row = gitoxide::read_row(repo, &id, &mut buf)?;
        for parent in &row.parents {
            if stop(parent) {
                continue;
            }
            let parent = oid_to_gix(parent)?;
            if seen.insert(parent) {
                stack.push(parent);
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

fn walk(repo: &gix::Repository, new_tips: &[RefTip], old_tips: &[RefTip]) -> Result<Vec<CommitRow>> {
    let tips = new_tips.iter().map(|tip| oid_to_gix(&tip.oid)).collect::<Result<Vec<_>>>()?;
    let mut hidden = Vec::new();
    for tip in old_tips {
        let id = oid_to_gix(&tip.oid)?;
        if hidden.contains(&id) {
            continue;
        }
        // Gone (gc'd after a rebase or branch delete) or no longer a commit-ish: nothing to hide.
        if let Some(object) = repo.try_find_object(id).map_err(gix_error)?
            && let Ok(commit) = object.peel_to_commit()
        {
            hidden.push(commit.id);
        }
    }
    reach_rows(repo, &tips, &hidden)
}

fn has_replace_refs(repo: &gix::Repository) -> Result<bool> {
    let platform = repo.references().map_err(gix_error)?;
    let mut replace = platform.prefixed("refs/replace/").map_err(gix_error)?;
    Ok(replace.next().is_some())
}

/// `true` when the cache must not be used and the engine should do a live walk: shallow clones (history
/// boundaries move on deepen/unshallow without any ref change), grafts (`info/grafts`), and replace refs
/// (`refs/replace/*`, which rewrite history without changing tips; libgit2 does not apply them, so a cache keyed on
/// tips could disagree with the git CLI).
pub fn should_bypass_cache<R: GixRepo + ?Sized>(repo: &R) -> Result<bool> {
    let repo = repo.gix()?;
    if repo.is_shallow().map_err(gix_error)? {
        return Ok(true);
    }
    match std::fs::metadata(repo.common_dir().join("info").join("grafts")) {
        Ok(_) => return Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    has_replace_refs(&repo)
}
