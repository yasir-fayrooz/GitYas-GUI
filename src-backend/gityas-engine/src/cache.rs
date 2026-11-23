//! Commit-graph cache integration (`CommitGraphService`).
//!
//! [`load_commits`] produces the history for `CommitsUpdated`:
//!
//! 1. Cache disabled (`EngineOptions::use_commit_cache = false`) or bypassed (shallow repo, grafts, replace refs:
//!    [`gityas_git::cache::should_bypass_cache`]) ⇒ live walk ([`gityas_git::commits::get_commits_live`]).
//! 2. Otherwise read the ref tips live and fingerprint them.
//!    * Equal to the in-memory hot entry of this engine (same repository) ⇒ return that `Arc` (no SQLite, no walk).
//!    * Equal to the stored fingerprint ⇒ serve from [`CacheDb::load_reachable`] (zero walking).
//!    * Different ⇒ `walk_new_commits(new tips, stored tips)`, `store_refresh` (one transaction), throttled `gc`,
//!      then `load_reachable`. A cold fill (no snapshot) walks everything and sorts the walked rows directly.
//! 3. The result is sorted with [`gityas_git::commits::sort_commits`], the same function the live walk uses.
//! 4. Never fails because of the cache: an incomplete graph (`!is_complete()`) purges the repository's rows and
//!    rebuilds from a full walk; any storage error is logged, the whole cache is `reset()`, and the history comes from
//!    a full walk which is then stored again. Only git errors (the live walk would fail too) are returned.
//!
//! References, tags and HEAD are always read live. Remote tag names are not part of it (network: `Engine::remote_tag_names`).
//!
//! [`CacheDb::load_reachable`]: gityas_storage::CacheDb::load_reachable

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use gityas_core::models::{Commit, CommitWrapper, Delta, RefTip, RefsFingerprint};
use gityas_core::{AppEvent, GitYasError, Oid, RepoId, Result};
use gityas_git::cache::{fingerprint, ref_tips, should_bypass_cache, walk_new_commits};
use gityas_git::commits::{get_commit_files_list, get_commits_live, retain_reachable, sort_commits};

use crate::session::SessionCtx;
use crate::{Engine, Inner};

#[cfg(test)]
mod bench;
#[cfg(test)]
mod tests;

/// In-process state of the commit-graph cache, one per engine (`Inner::commit_graph`).
#[derive(Default)]
pub(crate) struct CommitGraphState {
    /// History of the last cached load: served again without touching SQLite while the fingerprint is unchanged.
    hot: Mutex<Option<HotEntry>>,
    /// Last GC per repository (GC is throttled by `EngineOptions::cache_gc_interval`).
    last_gc: Mutex<HashMap<RepoId, Instant>>,
    pub(crate) stats: CacheStats,
    /// Test hook: the next cached load fails with a storage error before touching SQLite.
    #[cfg(test)]
    pub(crate) fail_next_load: std::sync::atomic::AtomicBool,
}

struct HotEntry {
    repo: RepoId,
    fingerprint: RefsFingerprint,
    commits: Arc<[Commit]>,
}

/// Counters for tests and diagnostics (monotonic; never reset).
#[derive(Default)]
pub(crate) struct CacheStats {
    /// Revision walks started (live, incremental or full).
    pub(crate) walks: AtomicU64,
    /// Commit rows produced by those walks.
    pub(crate) walked_commits: AtomicU64,
    /// `load_reachable` calls.
    pub(crate) sqlite_loads: AtomicU64,
    /// `gc` calls.
    pub(crate) gc_runs: AtomicU64,
    /// Full cache resets after a storage error.
    pub(crate) resets: AtomicU64,
}

impl CacheStats {
    fn bump(counter: &AtomicU64, by: u64) {
        counter.fetch_add(by, Ordering::Relaxed);
    }
}

/// Where a history came from (logged at debug level; asserted by tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// `use_commit_cache = false`: live walk.
    Disabled,
    /// Shallow / grafts / replace refs: live walk, cache untouched.
    Bypassed,
    /// Unchanged fingerprint, served from the in-memory hot entry.
    Hot,
    /// Unchanged fingerprint, served from SQLite without walking.
    Hit,
    /// No snapshot yet: full walk, stored.
    ColdFill,
    /// Refs changed: walked only the new commits, stored, loaded the reachable set.
    Incremental,
    /// The cache did not cover the history (`!is_complete()`): repository purged, full walk, stored.
    RebuiltIncomplete,
    /// A storage error: cache reset, full walk, stored again.
    RebuiltAfterError,
}

/// A history in display order and where it came from.
pub(crate) type History = (Arc<[Commit]>, Source);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Guards plain data only; a poisoned lock still holds a structurally valid value.
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// History + live refs for `CommitsUpdated`. Blocking; called with the command lock held (or from a watcher task).
pub(crate) fn load_commits(inner: &Inner, ctx: &SessionCtx, repo: &git2::Repository) -> Result<CommitWrapper> {
    let (commits, _) = load_history(inner, ctx, repo)?;
    Ok(CommitWrapper {
        commits,
        ..load_refs(repo)?
    })
}

/// A quick first page for `CommitsUpdated` (`partial`): the newest `GraphOptions::max_commits` commits, read
/// without walking the whole history, when this engine has no history of `repo` in memory (opening a repository or
/// switching back to it: the full load then goes through SQLite or a cold walk, which takes a while on a long
/// history). `None` when the full load is about as fast (hot entry, history no longer than the page) or on any
/// error (logged; the full load follows anyway).
pub(crate) fn preview_commits(inner: &Inner, ctx: &SessionCtx, repo: &git2::Repository) -> Option<CommitWrapper> {
    if lock(&inner.commit_graph.hot).as_ref().is_some_and(|hot| hot.repo == ctx.repo_id) {
        return None;
    }
    let preview = || -> Result<Option<CommitWrapper>> {
        let max = inner.storage.user().settings()?.graph.max_commits.max(1);
        let Some(commits) = gityas_git::commits::get_commits_newest(repo, max)? else {
            return Ok(None);
        };
        Ok(Some(CommitWrapper {
            partial: true,
            commits: Arc::from(commits),
            ..load_refs(repo)?
        }))
    };
    let started = Instant::now();
    match preview() {
        Ok(found) => {
            if let Some(found) = &found {
                tracing::debug!(
                    repo = %ctx.repo_id,
                    commits = found.commits.len(),
                    elapsed_ms = started.elapsed().as_millis(),
                    "history preview"
                );
            }
            found
        }
        Err(err) => {
            tracing::warn!(repo = %ctx.repo_id, "history preview failed; waiting for the full history: {err}");
            None
        }
    }
}

/// The live refs, tags and HEAD of a [`CommitWrapper`] (no commits).
fn load_refs(repo: &git2::Repository) -> Result<CommitWrapper> {
    Ok(CommitWrapper {
        commits: Arc::from([]),
        references: Arc::from(gityas_git::refs::list_references(repo)?),
        tags: Arc::from(gityas_git::refs::list_tags(repo)?),
        head: Some(gityas_git::commits::get_head(repo)?),
        partial: false,
    })
}

/// The history in display order and where it came from. See the module docs for the algorithm.
pub(crate) fn load_history(inner: &Inner, ctx: &SessionCtx, repo: &git2::Repository) -> Result<History> {
    let started = Instant::now();
    let result = load_history_inner(inner, ctx, repo);
    if let Ok((commits, source)) = &result {
        tracing::debug!(
            repo = %ctx.repo_id,
            ?source,
            commits = commits.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "history loaded"
        );
    }
    result
}

fn load_history_inner(inner: &Inner, ctx: &SessionCtx, repo: &git2::Repository) -> Result<History> {
    let state = &inner.commit_graph;
    if !inner.options.use_commit_cache {
        return Ok((live_walk(state, repo)?, Source::Disabled));
    }
    match should_bypass_cache(repo) {
        Ok(false) => {}
        Ok(true) => return Ok((live_walk(state, repo)?, Source::Bypassed)),
        Err(err) => {
            tracing::warn!(repo = %ctx.repo_id, "cannot tell whether to bypass the commit cache; live walk: {err}");
            return Ok((live_walk(state, repo)?, Source::Bypassed));
        }
    }

    let tips = ref_tips(repo)?;
    let fp = fingerprint(&tips);
    if let Some(commits) = hot_lookup(state, &ctx.repo_id, &fp) {
        return Ok((commits, Source::Hot));
    }

    let (commits, source) = match cached_history(inner, ctx, repo, &tips, &fp) {
        Ok(Some(found)) => found,
        Ok(None) => {
            tracing::info!(repo = %ctx.repo_id, "commit cache does not cover the history; rebuilding");
            if let Err(err) = inner.storage.cache().purge_repository(&ctx.repo_id) {
                tracing::warn!(repo = %ctx.repo_id, "purging the incomplete cache failed; resetting it: {err}");
                reset_cache(inner);
            }
            (rebuild(inner, ctx, repo, &tips, &fp)?, Source::RebuiltIncomplete)
        }
        Err(CacheFailure::Storage(err)) => {
            tracing::warn!(repo = %ctx.repo_id, "commit cache error; resetting it and walking live: {err}");
            reset_cache(inner);
            (rebuild(inner, ctx, repo, &tips, &fp)?, Source::RebuiltAfterError)
        }
        Err(CacheFailure::Git(err)) => return Err(err),
    };
    hot_store(state, &ctx.repo_id, fp, Arc::clone(&commits));
    Ok((commits, source))
}

/// Keeps git errors (returned to the caller) apart from storage errors (recovered from).
enum CacheFailure {
    Git(GitYasError),
    Storage(GitYasError),
}

/// `Ok(None)`: the cached graph is incomplete (treat as a miss).
fn cached_history(
    inner: &Inner,
    ctx: &SessionCtx,
    repo: &git2::Repository,
    tips: &[RefTip],
    fp: &RefsFingerprint,
) -> std::result::Result<Option<History>, CacheFailure> {
    let cache = inner.storage.cache();
    let state = &inner.commit_graph;
    #[cfg(test)]
    if state.fail_next_load.swap(false, Ordering::SeqCst) {
        return Err(CacheFailure::Storage(GitYasError::Storage("injected failure".into())));
    }
    let snapshot = cache.snapshot(&ctx.repo_id).map_err(CacheFailure::Storage)?;
    let source = match snapshot {
        Some((stored_fp, _)) if stored_fp == *fp => Source::Hit,
        Some((stored_fp, old_tips)) => {
            // Hot path: this engine's hot entry is exactly reach(old tips) (same fingerprint as the snapshot), so
            // walk only from the tips until known commits and extend it in memory; no SQLite reload. libgit2's
            // `hide` would pre-mark all of reach(old) instead, which is O(history).
            if let Some(previous) = hot_lookup(state, &ctx.repo_id, &stored_fp) {
                let known: HashSet<&Oid> = previous.iter().map(|c| &c.id).collect();
                let rows = walk_until_known(state, repo, tips, &known).map_err(CacheFailure::Git)?;
                store(inner, ctx, &rows, tips, fp).map_err(CacheFailure::Storage)?;
                if let Some(commits) = extend(&previous, &known, rows, tips, &old_tips) {
                    return Ok(Some((Arc::from(commits), Source::Incremental)));
                }
            } else {
                // Tips already in the snapshot add nothing to reach(new) ∖ reach(old); don't push them.
                let old_oids: HashSet<&Oid> = old_tips.iter().map(|tip| &tip.oid).collect();
                let changed: Vec<RefTip> = tips.iter().filter(|tip| !old_oids.contains(&tip.oid)).cloned().collect();
                let rows = walk(state, repo, &changed, &old_tips).map_err(CacheFailure::Git)?;
                store(inner, ctx, &rows, tips, fp).map_err(CacheFailure::Storage)?;
            }
            Source::Incremental
        }
        None => {
            // Cold fill: the walk already is reach(tips); no need to read it back.
            let rows = walk(state, repo, tips, &[]).map_err(CacheFailure::Git)?;
            store(inner, ctx, &rows, tips, fp).map_err(CacheFailure::Storage)?;
            return Ok(Some((Arc::from(sort_commits(rows)), Source::ColdFill)));
        }
    };
    let tip_oids: Vec<Oid> = tips.iter().map(|tip| tip.oid.clone()).collect();
    CacheStats::bump(&state.stats.sqlite_loads, 1);
    let graph = cache.load_reachable(&ctx.repo_id, &tip_oids).map_err(CacheFailure::Storage)?;
    if !graph.is_complete() {
        tracing::debug!(repo = %ctx.repo_id, missing = graph.missing.len(), "cached graph incomplete");
        return Ok(None);
    }
    Ok(Some((Arc::from(sort_commits(graph.commits)), source)))
}

/// `reach(tips)` from the previous history (= reach(`old_tips`), ids in `known`) plus the rows walked from the new
/// tips down to known commits, sorted. `None` if that set does not close over parents (not expected; the caller
/// then reloads from SQLite).
fn extend(previous: &[Commit], known: &HashSet<&Oid>, rows: Vec<Commit>, tips: &[RefTip], old_tips: &[RefTip]) -> Option<Vec<Commit>> {
    let row_ids: HashSet<&Oid> = rows.iter().map(|c| &c.id).collect();
    let present = |oid: &Oid| known.contains(oid) || row_ids.contains(oid);
    let closed = tips.iter().all(|tip| present(&tip.oid)) && rows.iter().all(|c| c.parents.iter().all(present));
    drop(row_ids);
    if !closed {
        tracing::warn!("in-memory history does not cover the new tips; reloading from the cache");
        return None;
    }
    // If every old tip is still a tip, reach(old) ⊆ reach(new): nothing to drop.
    let new_oids: HashSet<&Oid> = tips.iter().map(|tip| &tip.oid).collect();
    let shrank = old_tips.iter().any(|tip| !new_oids.contains(&tip.oid));
    drop(new_oids);
    let mut all = Vec::with_capacity(previous.len() + rows.len());
    all.extend_from_slice(previous);
    all.extend(rows);
    if shrank {
        let tip_oids: Vec<Oid> = tips.iter().map(|tip| tip.oid.clone()).collect();
        all = retain_reachable(all, &tip_oids);
    }
    Some(sort_commits(all))
}

/// Full walk from `tips`, stored as a fresh snapshot. A store failure is logged, never returned.
fn rebuild(inner: &Inner, ctx: &SessionCtx, repo: &git2::Repository, tips: &[RefTip], fp: &RefsFingerprint) -> Result<Arc<[Commit]>> {
    let rows = walk(&inner.commit_graph, repo, tips, &[])?;
    if let Err(err) = store(inner, ctx, &rows, tips, fp) {
        tracing::warn!(repo = %ctx.repo_id, "storing the rebuilt commit cache failed: {err}");
    }
    Ok(Arc::from(sort_commits(rows)))
}

/// `store_refresh` then (throttled) `gc`.
fn store(inner: &Inner, ctx: &SessionCtx, rows: &[Commit], tips: &[RefTip], fp: &RefsFingerprint) -> Result<()> {
    let cache = inner.storage.cache();
    cache.store_refresh(&ctx.repo_id, rows, tips, fp)?;
    let options = &inner.options;
    let due = {
        let mut last_gc = lock(&inner.commit_graph.last_gc);
        let due = last_gc
            .get(&ctx.repo_id)
            .is_none_or(|last| last.elapsed() >= options.cache_gc_interval);
        if due {
            last_gc.insert(ctx.repo_id.clone(), Instant::now());
        }
        due
    };
    if due {
        CacheStats::bump(&inner.commit_graph.stats.gc_runs, 1);
        let max_age = i64::try_from(options.cache_gc_max_age.as_secs()).unwrap_or(i64::MAX);
        cache.gc(&ctx.repo_id, max_age, options.cache_max_bytes)?;
    }
    Ok(())
}

fn walk(state: &CommitGraphState, repo: &git2::Repository, tips: &[RefTip], old_tips: &[RefTip]) -> Result<Vec<Commit>> {
    CacheStats::bump(&state.stats.walks, 1);
    let rows = walk_new_commits(repo, tips, old_tips)?;
    CacheStats::bump(&state.stats.walked_commits, rows.len() as u64);
    Ok(rows)
}

/// [`gityas_git::cache::walk_until_known`] with stats.
fn walk_until_known(state: &CommitGraphState, repo: &git2::Repository, tips: &[RefTip], known: &HashSet<&Oid>) -> Result<Vec<Commit>> {
    CacheStats::bump(&state.stats.walks, 1);
    let rows = gityas_git::cache::walk_until_known(repo, tips, known)?;
    CacheStats::bump(&state.stats.walked_commits, rows.len() as u64);
    Ok(rows)
}

fn live_walk(state: &CommitGraphState, repo: &git2::Repository) -> Result<Arc<[Commit]>> {
    CacheStats::bump(&state.stats.walks, 1);
    let commits = get_commits_live(repo)?;
    CacheStats::bump(&state.stats.walked_commits, commits.len() as u64);
    Ok(Arc::from(commits))
}

fn hot_lookup(state: &CommitGraphState, repo: &RepoId, fp: &RefsFingerprint) -> Option<Arc<[Commit]>> {
    lock(&state.hot)
        .as_ref()
        .filter(|hot| hot.repo == *repo && hot.fingerprint == *fp)
        .map(|hot| Arc::clone(&hot.commits))
}

fn hot_store(state: &CommitGraphState, repo: &RepoId, fingerprint: RefsFingerprint, commits: Arc<[Commit]>) {
    *lock(&state.hot) = Some(HotEntry {
        repo: repo.clone(),
        fingerprint,
        commits,
    });
}

/// Drops the hot entry (for `repo`, or any with `None`).
fn hot_clear(state: &CommitGraphState, repo: Option<&RepoId>) {
    let mut hot = lock(&state.hot);
    if repo.is_none_or(|repo| hot.as_ref().is_some_and(|h| h.repo == *repo)) {
        *hot = None;
    }
}

fn reset_cache(inner: &Inner) {
    CacheStats::bump(&inner.commit_graph.stats.resets, 1);
    hot_clear(&inner.commit_graph, None);
    lock(&inner.commit_graph.last_gc).clear();
    if let Err(err) = inner.storage.cache().reset() {
        tracing::warn!("resetting the commit cache failed: {err}");
    }
}

/// Drops every cached row for `repo` (called by `delete_repository`). Blocking.
pub(crate) fn purge_repository(inner: &Inner, repo: &RepoId) -> Result<()> {
    hot_clear(&inner.commit_graph, Some(repo));
    lock(&inner.commit_graph.last_gc).remove(repo);
    inner.storage.cache().purge_repository(repo)
}

/// Files changed by `commit` (vs its first parent), via the cache unless the cache is off or bypassed (grafts and
/// replace refs change parents, hence the diff). A cache read or write failure is logged, never returned.
fn commit_files_blocking(inner: &Inner, ctx: &SessionCtx, repo: &git2::Repository, commit: &Oid) -> Result<Vec<Delta>> {
    let use_cache = inner.options.use_commit_cache
        && match should_bypass_cache(repo) {
            Ok(bypass) => !bypass,
            Err(err) => {
                tracing::warn!(repo = %ctx.repo_id, "cannot tell whether to bypass the commit cache: {err}");
                false
            }
        };
    if !use_cache {
        return get_commit_files_list(repo, commit);
    }
    let cache = inner.storage.cache();
    match cache.commit_files(&ctx.repo_id, commit) {
        Ok(Some(files)) => return Ok(files),
        Ok(None) => {}
        Err(err) => tracing::warn!(repo = %ctx.repo_id, %commit, "reading cached commit files failed: {err}"),
    }
    let files = get_commit_files_list(repo, commit)?;
    if let Err(err) = cache.put_commit_files(&ctx.repo_id, commit, &files) {
        tracing::warn!(repo = %ctx.repo_id, %commit, "caching commit files failed: {err}");
    }
    Ok(files)
}

impl Engine {
    /// Recomputes history via the commit-graph cache and emits `CommitsUpdated`.
    pub async fn refresh_commits(&self) -> Result<CommitWrapper> {
        self.with_repo(|inner, ctx, repo| {
            let commits = load_commits(inner, ctx, repo)?;
            inner.emit_scoped(
                ctx,
                AppEvent::CommitsUpdated {
                    repo: ctx.repo_id.clone(),
                    commits: commits.clone(),
                },
            );
            Ok(commits)
        })
        .await
    }

    /// Files changed by a commit (vs first parent), served from the cache when present.
    pub async fn commit_files(&self, commit: Oid) -> Result<Vec<Delta>> {
        self.with_repo(move |inner, ctx, repo| commit_files_blocking(inner, ctx, repo, &commit))
            .await
    }
}
