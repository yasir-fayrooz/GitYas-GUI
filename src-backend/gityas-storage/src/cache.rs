use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gityas_core::models::{CommitRow, Delta, DeltaChange, RefTip, RefsFingerprint};
use gityas_core::{GitYasError, Oid, RepoId, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::db::{self, db, lock};
use crate::schema::{self, CACHE_VERSION};

/// Cache DB DAO: one writer + one reader connection, so UI reads never wait behind a bulk insert (WAL).
/// Disposable: callers fall back to a live walk on any error. Inserts are idempotent (`INSERT OR IGNORE`), so
/// concurrent processes are harmless.
#[derive(Clone)]
pub struct CacheDb {
    inner: Arc<CacheInner>,
}

struct CacheInner {
    writer: Mutex<Connection>,
    reader: Mutex<Connection>,
    location: Location,
}

#[derive(Clone)]
enum Location {
    File(PathBuf),
    /// Named memdb database (shared by the two connections of this `CacheDb` only).
    Memory(String),
}

/// Commits reachable from a set of tips, loaded from cached parent links into an interned graph.
///
/// **Order:** `commits` is in breadth-first discovery order from the tips, which is *not* a display order;
/// callers must sort (the engine applies the same sort as the live walk).
#[derive(Debug, Clone, Default)]
pub struct ReachableGraph {
    pub commits: Vec<CommitRow>,
    /// Tips or parents that are referenced but have no cached row. Non-empty means the cache does not cover the
    /// requested history: treat it as a miss (live walk + rebuild), never show the partial list as complete.
    pub missing: Vec<Oid>,
}

impl ReachableGraph {
    /// True when every commit reachable from the tips was found in the cache.
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty()
    }
}

impl CacheDb {
    /// Opens the cache file; on a schema mismatch or any open error the file is deleted and recreated
    /// (silently apart from a log line; the cache is disposable).
    pub(crate) fn open_file(path: &Path) -> Result<CacheDb> {
        let location = Location::File(path.to_path_buf());
        match open_connections(&location) {
            Ok(conns) => Ok(Self::from_parts(conns, location)),
            Err(err) => {
                tracing::info!(path = %path.display(), %err, "recreating cache database");
                db::remove_db_files(path)?;
                let conns = open_connections(&location)?;
                Ok(Self::from_parts(conns, location))
            }
        }
    }

    pub(crate) fn open_in_memory() -> Result<CacheDb> {
        let location = Location::Memory(format!("gityas-cache-{}", nanoid::nanoid!()));
        let conns = open_connections(&location)?;
        Ok(Self::from_parts(conns, location))
    }

    fn from_parts((writer, reader): (Connection, Connection), location: Location) -> CacheDb {
        CacheDb {
            inner: Arc::new(CacheInner {
                writer: Mutex::new(writer),
                reader: Mutex::new(reader),
                location,
            }),
        }
    }

    /// Stored fingerprint and ref-tip snapshot (sorted by refname) from the last refresh, if any.
    pub fn snapshot(&self, repo: &RepoId) -> Result<Option<(RefsFingerprint, Vec<RefTip>)>> {
        let mut conn = lock(&self.inner.reader);
        let tx = conn.transaction().map_err(db)?;
        let fingerprint: Option<Vec<u8>> = tx
            .query_row("SELECT refs_fingerprint FROM repo_cache WHERE repo_id = ?1", [repo], |row| {
                row.get(0)
            })
            .optional()
            .map_err(db)?;
        let Some(fingerprint) = fingerprint else {
            return Ok(None);
        };
        let tips = {
            let mut stmt = tx
                .prepare_cached("SELECT refname, oid FROM ref_tips WHERE repo_id = ?1 ORDER BY refname")
                .map_err(db)?;
            let mut rows = stmt.query([repo]).map_err(db)?;
            let mut tips = Vec::new();
            while let Some(row) = rows.next().map_err(db)? {
                tips.push(RefTip {
                    refname: row.get(0).map_err(db)?,
                    oid: oid_at(row, 1)?,
                });
            }
            tips
        };
        Ok(Some((RefsFingerprint(fingerprint), tips)))
    }

    /// In one transaction: insert `commits` (and their parents) idempotently, then replace the ref-tip snapshot
    /// and fingerprint. Precondition: after this call every commit reachable from `tips` is cached, i.e.
    /// `commits` ⊇ reach(tips) ∖ reach(previous snapshot tips).
    pub fn store_refresh(&self, repo: &RepoId, commits: &[CommitRow], tips: &[RefTip], fingerprint: &RefsFingerprint) -> Result<()> {
        let now = db::unix_now()?;
        let mut conn = lock(&self.inner.writer);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        {
            let mut insert_commit = tx
                .prepare_cached(
                    "INSERT OR IGNORE INTO commits
                     (repo_id, oid, author_name, author_email, author_time, committer_time, summary, lossy, description,
                      unreachable_since)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)",
                )
                .map_err(db)?;
            let mut insert_parent = tx
                .prepare_cached("INSERT OR IGNORE INTO commit_parents (repo_id, oid, idx, parent_oid) VALUES (?1, ?2, ?3, ?4)")
                .map_err(db)?;
            for commit in commits {
                insert_commit
                    .execute(params![
                        repo,
                        commit.id.as_bytes(),
                        commit.author_name,
                        commit.author_email,
                        commit.author_time,
                        commit.committer_time,
                        commit.summary,
                        commit.lossy,
                        commit.description,
                    ])
                    .map_err(db)?;
                for (idx, parent) in commit.parents.iter().enumerate() {
                    let idx = i64::try_from(idx).map_err(|_| GitYasError::InvalidInput("too many parents".into()))?;
                    insert_parent
                        .execute(params![repo, commit.id.as_bytes(), idx, parent.as_bytes()])
                        .map_err(db)?;
                }
            }
        }
        tx.execute("DELETE FROM ref_tips WHERE repo_id = ?1", [repo]).map_err(db)?;
        {
            let mut insert_tip = tx
                .prepare_cached("INSERT INTO ref_tips (repo_id, refname, oid) VALUES (?1, ?2, ?3)")
                .map_err(db)?;
            for tip in tips {
                insert_tip.execute(params![repo, tip.refname, tip.oid.as_bytes()]).map_err(db)?;
            }
        }
        tx.execute(
            "INSERT INTO repo_cache (repo_id, refs_fingerprint, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(repo_id) DO UPDATE SET refs_fingerprint = excluded.refs_fingerprint, updated_at = excluded.updated_at",
            params![repo, fingerprint.0, now],
        )
        .map_err(db)?;
        tx.commit().map_err(db)
    }

    /// Commits reachable from `tips` via cached parent links (in-memory BFS over an interned `u32` graph).
    /// See [`ReachableGraph`] for the ordering contract and how to detect an incomplete cache.
    pub fn load_reachable(&self, repo: &RepoId, tips: &[Oid]) -> Result<ReachableGraph> {
        let mut conn = lock(&self.inner.reader);
        let tx = conn.transaction().map_err(db)?;
        let graph = Graph::load(&tx, repo, true)?;
        drop(tx);
        drop(conn);

        let tip_ids: Vec<Option<u32>> = tips.iter().map(|tip| graph.id(tip.as_bytes())).collect();
        let mut missing: Vec<Oid> = tips
            .iter()
            .zip(&tip_ids)
            .filter(|(_, id)| id.is_none())
            .map(|(tip, _)| tip.clone())
            .collect();
        let order = graph.bfs(tip_ids.into_iter().flatten());
        let mut commits = Vec::with_capacity(order.len());
        for id in order {
            let node = id as usize;
            match graph.meta.get(node).and_then(Option::as_ref) {
                Some(meta) => commits.push(CommitRow {
                    id: graph.oids[node].clone(),
                    parents: graph.parents[node].iter().map(|&p| graph.oids[p as usize].clone()).collect(),
                    author_name: meta.author_name.clone(),
                    author_email: meta.author_email.clone(),
                    author_time: meta.author_time,
                    committer_time: meta.committer_time,
                    summary: meta.summary.clone(),
                    description: meta.description.clone(),
                    lossy: meta.lossy,
                }),
                None => missing.push(graph.oids[node].clone()),
            }
        }
        missing.sort();
        missing.dedup();
        Ok(ReachableGraph { commits, missing })
    }

    /// Cached file list of a commit; `None` if not cached (an empty list is a valid cached value).
    pub fn commit_files(&self, repo: &RepoId, commit: &Oid) -> Result<Option<Vec<Delta>>> {
        let mut conn = lock(&self.inner.reader);
        let tx = conn.transaction().map_err(db)?;
        let cached = tx
            .prepare_cached("SELECT 1 FROM commit_file_lists WHERE repo_id = ?1 AND oid = ?2")
            .map_err(db)?
            .exists(params![repo, commit.as_bytes()])
            .map_err(db)?;
        if !cached {
            return Ok(None);
        }
        let mut stmt = tx
            .prepare_cached("SELECT path, old_path, status FROM commit_files WHERE repo_id = ?1 AND oid = ?2 ORDER BY idx")
            .map_err(db)?;
        let mut rows = stmt.query(params![repo, commit.as_bytes()]).map_err(db)?;
        let mut files = Vec::new();
        while let Some(row) = rows.next().map_err(db)? {
            let path = bytes_to_path(row.get_ref(0).map_err(db)?.as_blob().map_err(|e| db(e.into()))?)?;
            let old_path = match row.get_ref(1).map_err(db)?.as_blob_or_null().map_err(|e| db(e.into()))? {
                Some(bytes) => Some(bytes_to_path(bytes)?),
                None => None,
            };
            let change = change_from_code(row.get(2).map_err(db)?)?;
            files.push(Delta { path, old_path, change });
        }
        Ok(Some(files))
    }

    /// Stores (replaces) the file list of a commit.
    pub fn put_commit_files(&self, repo: &RepoId, commit: &Oid, files: &[Delta]) -> Result<()> {
        let encoded = files
            .iter()
            .map(|d| {
                Ok((
                    path_to_bytes(&d.path)?,
                    d.old_path.as_deref().map(path_to_bytes).transpose()?,
                    change_code(d.change),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let now = db::unix_now()?;
        let mut conn = lock(&self.inner.writer);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        tx.execute(
            "DELETE FROM commit_files WHERE repo_id = ?1 AND oid = ?2",
            params![repo, commit.as_bytes()],
        )
        .map_err(db)?;
        tx.execute(
            "INSERT INTO commit_file_lists (repo_id, oid, cached_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(repo_id, oid) DO UPDATE SET cached_at = excluded.cached_at",
            params![repo, commit.as_bytes(), now],
        )
        .map_err(db)?;
        {
            let mut insert = tx
                .prepare_cached("INSERT INTO commit_files (repo_id, oid, idx, path, old_path, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")
                .map_err(db)?;
            for (idx, (path, old_path, status)) in encoded.iter().enumerate() {
                let idx = i64::try_from(idx).map_err(|_| GitYasError::InvalidInput("too many files".into()))?;
                insert
                    .execute(params![repo, commit.as_bytes(), idx, path, old_path, status])
                    .map_err(db)?;
            }
        }
        tx.commit().map_err(db)
    }

    /// Garbage collection for one repository, relative to the **stored** ref-tip snapshot (call it after
    /// `store_refresh`):
    ///
    /// 1. Reachability is recomputed; newly unreachable commits get `unreachable_since = now`, commits that became
    ///    reachable again are cleared. Commits unreachable for at least `max_age_secs` are deleted (with their
    ///    parent links and file lists); `max_age_secs = 0` deletes all unreachable commits. Orphan file lists
    ///    (commit not cached) older than `max_age_secs` are deleted too.
    /// 2. If the database still uses more than `max_bytes`: all unreachable commits and all file lists of this
    ///    repository are dropped, then other repositories' caches are purged, least recently refreshed first.
    ///    Reachable commits of `repo` are never deleted (that would force a full re-walk).
    /// 3. Free pages are returned to the OS (`incremental_vacuum`).
    pub fn gc(&self, repo: &RepoId, max_age_secs: i64, max_bytes: u64) -> Result<()> {
        let now = db::unix_now()?;
        let cutoff = now.saturating_sub(max_age_secs.max(0));
        let mut conn = lock(&self.inner.writer);

        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        let deleted = mark_and_sweep(&tx, repo, now, Some(cutoff))?;
        tx.execute(
            "DELETE FROM commit_file_lists WHERE repo_id = ?1 AND cached_at <= ?2
             AND NOT EXISTS (SELECT 1 FROM commits c WHERE c.repo_id = commit_file_lists.repo_id AND c.oid = commit_file_lists.oid)",
            params![repo, cutoff],
        )
        .map_err(db)?;
        delete_orphan_files(&tx, repo)?;
        tx.commit().map_err(db)?;
        tracing::debug!(%repo, deleted, "cache gc (age)");

        if used_bytes(&conn)? > max_bytes {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
            let deleted = mark_and_sweep(&tx, repo, now, None)?;
            tx.execute("DELETE FROM commit_files WHERE repo_id = ?1", [repo]).map_err(db)?;
            tx.execute("DELETE FROM commit_file_lists WHERE repo_id = ?1", [repo]).map_err(db)?;
            tx.commit().map_err(db)?;
            tracing::debug!(%repo, deleted, "cache gc (size): dropped unreachable commits and file lists");

            while used_bytes(&conn)? > max_bytes {
                let victim: Option<String> = conn
                    .query_row(
                        "SELECT repo_id FROM (
                             SELECT repo_id, updated_at FROM repo_cache
                             UNION ALL SELECT DISTINCT repo_id, 0 FROM commits
                             UNION ALL SELECT DISTINCT repo_id, 0 FROM commit_file_lists)
                         WHERE repo_id <> ?1 GROUP BY repo_id ORDER BY MAX(updated_at), repo_id LIMIT 1",
                        [repo],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(db)?;
                let Some(victim) = victim else { break };
                tracing::debug!(%repo, %victim, "cache gc (size): purging another repository");
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
                purge(&tx, &victim)?;
                tx.commit().map_err(db)?;
            }
        }
        conn.pragma_query(None, "incremental_vacuum", |_| Ok(())).map_err(db)?;
        Ok(())
    }

    /// Removes every cached row of a repository (on `delete_repository`).
    pub fn purge_repository(&self, repo: &RepoId) -> Result<()> {
        let mut conn = lock(&self.inner.writer);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        purge(&tx, repo)?;
        tx.commit().map_err(db)
    }

    /// Drops the whole cache. For a file DB the file is deleted and recreated (works even if it is corrupt);
    /// use after an unexpected cache error. Other `CacheDb` clones see the fresh DB.
    pub fn reset(&self) -> Result<()> {
        let mut writer = lock(&self.inner.writer);
        let mut reader = lock(&self.inner.reader);
        match &self.inner.location {
            Location::File(path) => {
                // Close both connections (placeholders are dropped right after) before deleting the files.
                *reader = Connection::open_in_memory().map_err(db)?;
                *writer = Connection::open_in_memory().map_err(db)?;
                db::remove_db_files(path)?;
                let (w, r) = open_connections(&self.inner.location)?;
                *writer = w;
                *reader = r;
            }
            Location::Memory(_) => {
                let tx = writer.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
                for table in TABLES {
                    tx.execute(&format!("DELETE FROM {table}"), []).map_err(db)?;
                }
                tx.commit().map_err(db)?;
            }
        }
        Ok(())
    }

    /// Database file, `None` for in-memory storage.
    pub fn path(&self) -> Option<PathBuf> {
        match &self.inner.location {
            Location::File(path) => Some(path.clone()),
            Location::Memory(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_writer<T>(&self, f: impl FnOnce(&mut Connection) -> T) -> T {
        f(&mut lock(&self.inner.writer))
    }

    #[cfg(test)]
    pub(crate) fn with_reader<T>(&self, f: impl FnOnce(&mut Connection) -> T) -> T {
        f(&mut lock(&self.inner.reader))
    }
}

const TABLES: [&str; 6] = [
    "repo_cache",
    "ref_tips",
    "commits",
    "commit_parents",
    "commit_file_lists",
    "commit_files",
];

fn open_connection(location: &Location) -> rusqlite::Result<Connection> {
    match location {
        Location::File(path) => Connection::open_with_flags(path, db::file_flags()),
        Location::Memory(name) => Connection::open_with_flags(db::memdb_uri(name), db::memdb_flags()),
    }
}

/// Opens writer + reader, creating or validating the schema. Errors (including a version mismatch) are returned
/// to the caller, which deletes and recreates the file.
fn open_connections(location: &Location) -> Result<(Connection, Connection)> {
    let wal = matches!(location, Location::File(_));
    let mut writer = open_connection(location).map_err(db)?;
    writer.busy_timeout(db::BUSY_TIMEOUT).map_err(db)?;
    let version: i64 = writer.pragma_query_value(None, "user_version", |row| row.get(0)).map_err(db)?;
    if version != 0 && version != CACHE_VERSION {
        return Err(GitYasError::Storage(format!(
            "cache schema version {version}, expected {CACHE_VERSION}"
        )));
    }
    if version == 0 {
        // Only effective before the first table exists; lets gc hand freed pages back to the OS.
        writer.pragma_update(None, "auto_vacuum", "INCREMENTAL").map_err(db)?;
    }
    db::configure(&writer, wal).map_err(db)?;
    schema::cache_migrations().to_latest(&mut writer).map_err(db::migration)?;

    let reader = open_connection(location).map_err(db)?;
    db::configure(&reader, wal).map_err(db)?;
    reader.pragma_update(None, "query_only", true).map_err(db)?;
    reader.query_row("SELECT count(*) FROM repo_cache", [], |_| Ok(())).map_err(db)?;
    Ok((writer, reader))
}

struct Meta {
    author_name: String,
    author_email: String,
    author_time: i64,
    committer_time: i64,
    summary: String,
    description: String,
    lossy: bool,
}

/// Interned commit graph of one repository: node ids are `u32` indexes into the parallel vectors.
#[derive(Default)]
struct Graph {
    index: HashMap<Box<[u8]>, u32>,
    oids: Vec<Oid>,
    /// `None` for nodes only referenced as a parent (no cached row).
    meta: Vec<Option<Meta>>,
    unreachable_since: Vec<Option<i64>>,
    parents: Vec<Vec<u32>>,
}

impl Graph {
    /// Loads all commits and parent links of `repo`. `with_meta = false` skips the text columns (gc).
    fn load(tx: &Transaction<'_>, repo: &RepoId, with_meta: bool) -> Result<Graph> {
        let mut graph = Graph::default();
        {
            let sql = if with_meta {
                "SELECT oid, unreachable_since, author_name, author_email, author_time, committer_time, summary, lossy,
                        description
                 FROM commits WHERE repo_id = ?1"
            } else {
                "SELECT oid, unreachable_since FROM commits WHERE repo_id = ?1"
            };
            let mut stmt = tx.prepare_cached(sql).map_err(db)?;
            let mut rows = stmt.query([repo]).map_err(db)?;
            while let Some(row) = rows.next().map_err(db)? {
                let id = graph.intern(blob_at(row, 0)?)? as usize;
                graph.unreachable_since[id] = row.get(1).map_err(db)?;
                graph.meta[id] = Some(if with_meta {
                    Meta {
                        author_name: row.get(2).map_err(db)?,
                        author_email: row.get(3).map_err(db)?,
                        author_time: row.get(4).map_err(db)?,
                        committer_time: row.get(5).map_err(db)?,
                        summary: row.get(6).map_err(db)?,
                        lossy: row.get(7).map_err(db)?,
                        description: row.get(8).map_err(db)?,
                    }
                } else {
                    Meta {
                        author_name: String::new(),
                        author_email: String::new(),
                        author_time: 0,
                        committer_time: 0,
                        summary: String::new(),
                        description: String::new(),
                        lossy: false,
                    }
                });
            }
        }
        {
            // PK order (oid, idx), so each child's parents are appended in parent order.
            let mut stmt = tx
                .prepare_cached("SELECT oid, parent_oid FROM commit_parents WHERE repo_id = ?1 ORDER BY oid, idx")
                .map_err(db)?;
            let mut rows = stmt.query([repo]).map_err(db)?;
            while let Some(row) = rows.next().map_err(db)? {
                let child = graph.intern(blob_at(row, 0)?)?;
                let parent = graph.intern(blob_at(row, 1)?)?;
                graph.parents[child as usize].push(parent);
            }
        }
        Ok(graph)
    }

    fn intern(&mut self, bytes: &[u8]) -> Result<u32> {
        if let Some(&id) = self.index.get(bytes) {
            return Ok(id);
        }
        let id = u32::try_from(self.oids.len()).map_err(|_| GitYasError::Storage("commit graph too large".into()))?;
        self.oids
            .push(Oid::from_bytes(bytes).map_err(|err| GitYasError::Storage(format!("bad cached oid: {err}")))?);
        self.index.insert(bytes.into(), id);
        self.meta.push(None);
        self.unreachable_since.push(None);
        self.parents.push(Vec::new());
        Ok(id)
    }

    fn id(&self, bytes: &[u8]) -> Option<u32> {
        self.index.get(bytes).copied()
    }

    /// Nodes reachable from `starts`, in BFS discovery order (each once).
    fn bfs(&self, starts: impl IntoIterator<Item = u32>) -> Vec<u32> {
        let mut seen = vec![false; self.oids.len()];
        let mut queue = VecDeque::new();
        let mut order = Vec::new();
        for start in starts {
            if let Some(flag) = seen.get_mut(start as usize)
                && !*flag
            {
                *flag = true;
                queue.push_back(start);
            }
        }
        while let Some(node) = queue.pop_front() {
            order.push(node);
            for &parent in &self.parents[node as usize] {
                if let Some(flag) = seen.get_mut(parent as usize)
                    && !*flag
                {
                    *flag = true;
                    queue.push_back(parent);
                }
            }
        }
        order
    }
}

/// Updates `unreachable_since` from the stored tips and deletes unreachable commits whose mark is `<= cutoff`
/// (`cutoff = None`: delete every unreachable commit). Returns the number of deleted commits.
fn mark_and_sweep(tx: &Transaction<'_>, repo: &RepoId, now: i64, cutoff: Option<i64>) -> Result<usize> {
    let graph = Graph::load(tx, repo, false)?;
    let tips: Vec<u32> = {
        let mut stmt = tx.prepare_cached("SELECT oid FROM ref_tips WHERE repo_id = ?1").map_err(db)?;
        let mut rows = stmt.query([repo]).map_err(db)?;
        let mut tips = Vec::new();
        while let Some(row) = rows.next().map_err(db)? {
            if let Some(id) = graph.id(blob_at(row, 0)?) {
                tips.push(id);
            }
        }
        tips
    };
    let mut reachable = vec![false; graph.oids.len()];
    for id in graph.bfs(tips) {
        reachable[id as usize] = true;
    }

    let mut set_mark = tx
        .prepare_cached("UPDATE commits SET unreachable_since = ?3 WHERE repo_id = ?1 AND oid = ?2")
        .map_err(db)?;
    let mut doomed = Vec::new();
    for (node, oid) in graph.oids.iter().enumerate() {
        if graph.meta[node].is_none() {
            continue; // referenced parent without a row
        }
        let since = graph.unreachable_since[node];
        if reachable[node] {
            if since.is_some() {
                set_mark.execute(params![repo, oid.as_bytes(), None::<i64>]).map_err(db)?;
            }
            continue;
        }
        let since = match since {
            Some(since) => since,
            None => {
                set_mark.execute(params![repo, oid.as_bytes(), now]).map_err(db)?;
                now
            }
        };
        if cutoff.is_none_or(|cutoff| since <= cutoff) {
            doomed.push(oid);
        }
    }
    let mut deletes = Vec::with_capacity(4);
    for table in ["commits", "commit_parents", "commit_files", "commit_file_lists"] {
        deletes.push(
            tx.prepare_cached(&format!("DELETE FROM {table} WHERE repo_id = ?1 AND oid = ?2"))
                .map_err(db)?,
        );
    }
    for oid in &doomed {
        for stmt in &mut deletes {
            stmt.execute(params![repo, oid.as_bytes()]).map_err(db)?;
        }
    }
    Ok(doomed.len())
}

fn delete_orphan_files(tx: &Transaction<'_>, repo: &RepoId) -> Result<()> {
    tx.execute(
        "DELETE FROM commit_files WHERE repo_id = ?1
         AND NOT EXISTS (SELECT 1 FROM commit_file_lists l WHERE l.repo_id = commit_files.repo_id AND l.oid = commit_files.oid)",
        [repo],
    )
    .map_err(db)?;
    Ok(())
}

fn purge(tx: &Transaction<'_>, repo: &str) -> Result<()> {
    for table in TABLES {
        tx.execute(&format!("DELETE FROM {table} WHERE repo_id = ?1"), [repo]).map_err(db)?;
    }
    Ok(())
}

fn used_bytes(conn: &Connection) -> Result<u64> {
    let page_count: i64 = conn.pragma_query_value(None, "page_count", |row| row.get(0)).map_err(db)?;
    let free: i64 = conn.pragma_query_value(None, "freelist_count", |row| row.get(0)).map_err(db)?;
    let page_size: i64 = conn.pragma_query_value(None, "page_size", |row| row.get(0)).map_err(db)?;
    let used = page_count.saturating_sub(free).saturating_mul(page_size);
    u64::try_from(used.max(0)).map_err(|_| GitYasError::Storage("database size out of range".into()))
}

fn blob_at<'r>(row: &'r rusqlite::Row<'_>, idx: usize) -> Result<&'r [u8]> {
    row.get_ref(idx).map_err(db)?.as_blob().map_err(|err| db(err.into()))
}

fn oid_at(row: &rusqlite::Row<'_>, idx: usize) -> Result<Oid> {
    Oid::from_bytes(blob_at(row, idx)?).map_err(|err| GitYasError::Storage(format!("bad cached oid: {err}")))
}

/// Stable on-disk codes for `DeltaChange` (never reuse a number).
fn change_code(change: DeltaChange) -> i64 {
    match change {
        DeltaChange::Added => 1,
        DeltaChange::Untracked => 2,
        DeltaChange::Copied => 3,
        DeltaChange::Deleted => 4,
        DeltaChange::Renamed => 5,
        DeltaChange::Modified => 6,
        DeltaChange::Conflicted => 7,
        DeltaChange::Typechange => 8,
        DeltaChange::Ignored => 9,
        DeltaChange::Unreadable => 10,
    }
}

fn change_from_code(code: i64) -> Result<DeltaChange> {
    Ok(match code {
        1 => DeltaChange::Added,
        2 => DeltaChange::Untracked,
        3 => DeltaChange::Copied,
        4 => DeltaChange::Deleted,
        5 => DeltaChange::Renamed,
        6 => DeltaChange::Modified,
        7 => DeltaChange::Conflicted,
        8 => DeltaChange::Typechange,
        9 => DeltaChange::Ignored,
        10 => DeltaChange::Unreadable,
        other => return Err(GitYasError::Storage(format!("unknown cached file status {other}"))),
    })
}

#[cfg(unix)]
fn path_to_bytes(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Ok(path.as_os_str().as_bytes().to_vec())
}

#[cfg(unix)]
fn bytes_to_path(bytes: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

/// Non-Unix: paths from git are UTF-8 there; anything else is rejected rather than stored lossily.
#[cfg(not(unix))]
fn path_to_bytes(path: &Path) -> Result<Vec<u8>> {
    path.to_str()
        .map(|s| s.as_bytes().to_vec())
        .ok_or_else(|| GitYasError::InvalidInput(format!("path is not valid Unicode: {}", path.display())))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: &[u8]) -> Result<PathBuf> {
    std::str::from_utf8(bytes)
        .map(PathBuf::from)
        .map_err(|_| GitYasError::Storage("cached path is not valid UTF-8".into()))
}
