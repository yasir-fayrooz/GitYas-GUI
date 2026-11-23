//! Storage-level tests: open/migrate/recover on real files, cross-connection behaviour, cache DAO.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use gityas_core::models::{CommitRow, Delta, DeltaChange, RefTip, RefsFingerprint};
use gityas_core::{GitYasError, Oid, RepoId};
use rusqlite::TransactionBehavior;
use tempfile::TempDir;

use crate::{CACHE_DB_FILE, OpenReport, Storage, StoragePaths, USER_DB_FILE, db};

fn paths(dir: &TempDir) -> StoragePaths {
    StoragePaths {
        data_dir: dir.path().join("data"),
        cache_dir: dir.path().join("cache"),
    }
}

fn open(dir: &TempDir) -> (Storage, OpenReport) {
    Storage::open(&paths(dir)).unwrap()
}

fn oid(n: u32) -> Oid {
    let mut bytes = [0u8; 20];
    bytes[..4].copy_from_slice(&n.to_be_bytes());
    bytes[19] = 1; // never all-zero
    Oid::from_bytes(&bytes).unwrap()
}

fn commit(n: u32, parents: &[u32]) -> CommitRow {
    CommitRow {
        id: oid(n),
        parents: parents.iter().map(|&p| oid(p)).collect(),
        author_name: format!("author {n}"),
        author_email: format!("a{n}@example.com"),
        author_time: 1_000 + i64::from(n),
        committer_time: 2_000 + i64::from(n),
        summary: format!("commit {n}"),
        description: String::new(),
        lossy: n % 7 == 3,
    }
}

fn tip(name: &str, n: u32) -> RefTip {
    RefTip {
        refname: name.into(),
        oid: oid(n),
    }
}

fn fp(byte: u8) -> RefsFingerprint {
    RefsFingerprint(vec![byte; 8])
}

fn repo(name: &str) -> RepoId {
    name.to_owned()
}

fn sorted_ids(commits: &[CommitRow]) -> Vec<Oid> {
    let mut ids: Vec<Oid> = commits.iter().map(|c| c.id.clone()).collect();
    ids.sort();
    ids
}

fn ids(ns: &[u32]) -> Vec<Oid> {
    let mut v: Vec<Oid> = ns.iter().map(|&n| oid(n)).collect();
    v.sort();
    v
}

fn count(storage: &Storage, table: &str) -> i64 {
    storage
        .cache()
        .with_reader(|c| c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)))
        .unwrap()
}

// ---------------------------------------------------------------- open / migrate / recover

#[test]
fn storage_is_clone_send_sync() {
    fn assert_traits<T: Clone + Send + Sync + 'static>() {}
    assert_traits::<Storage>();
    assert_traits::<crate::UserDb>();
    assert_traits::<crate::CacheDb>();
}

#[test]
fn migrations_from_empty_directory() {
    let dir = TempDir::new().unwrap();
    let (storage, report) = open(&dir);
    assert_eq!(report, OpenReport::default());
    let user_path = dir.path().join("data").join(USER_DB_FILE);
    let cache_path = dir.path().join("cache").join(CACHE_DB_FILE);
    assert_eq!(storage.user().path(), Some(user_path.clone()));
    assert_eq!(storage.cache().path(), Some(cache_path.clone()));
    assert!(user_path.exists() && cache_path.exists());

    storage.user().with_conn(|c| {
        assert_eq!(db::journal_mode(c).unwrap(), "wal");
        let version: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 1);
        let fk: i64 = c.pragma_query_value(None, "foreign_keys", |r| r.get(0)).unwrap();
        assert_eq!(fk, 1);
        let sync: i64 = c.pragma_query_value(None, "synchronous", |r| r.get(0)).unwrap();
        assert_eq!(sync, 1, "NORMAL");
    });
    storage.cache().with_writer(|c| {
        assert_eq!(db::journal_mode(c).unwrap(), "wal");
        let version: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, crate::schema::CACHE_VERSION);
        let auto_vacuum: i64 = c.pragma_query_value(None, "auto_vacuum", |r| r.get(0)).unwrap();
        assert_eq!(auto_vacuum, 2, "INCREMENTAL");
    });
    storage.cache().with_reader(|c| {
        let ro: i64 = c.pragma_query_value(None, "query_only", |r| r.get(0)).unwrap();
        assert_eq!(ro, 1);
    });

    // Reopening keeps the data and reports nothing.
    let repo = storage.user().insert_repository("a", Path::new("/r/a"), None).unwrap();
    storage
        .cache()
        .store_refresh(&repo.id, &[commit(1, &[])], &[tip("refs/heads/main", 1)], &fp(1))
        .unwrap();
    drop(storage);
    let (storage, report) = open(&dir);
    assert!(report.warnings.is_empty());
    assert_eq!(storage.user().repositories().unwrap(), vec![repo.clone()]);
    assert!(storage.cache().snapshot(&repo.id).unwrap().is_some());
}

fn corrupt_files(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().contains(".corrupt-"))
        .collect();
    found.sort();
    found
}

#[test]
fn corrupt_user_db_is_quarantined_and_recreated() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let garbage = b"this is definitely not an SQLite database, just garbage bytes................................".repeat(64);
    std::fs::write(data.join(USER_DB_FILE), &garbage).unwrap();
    std::fs::write(data.join("gityas.db-wal"), b"junk wal").unwrap();

    let (storage, report) = open(&dir);
    assert_eq!(report.warnings.len(), 1, "{report:?}");
    assert!(report.warnings[0].contains("corrupt-"), "{}", report.warnings[0]);

    let moved = corrupt_files(&data);
    assert_eq!(moved.len(), 2, "{moved:?}");
    let main = moved.iter().find(|p| !p.to_string_lossy().ends_with("-wal")).unwrap();
    assert_eq!(std::fs::read(main).unwrap(), garbage, "original bytes are preserved");
    assert!(moved.iter().any(|p| p.to_string_lossy().ends_with("-wal")));

    // The fresh DB works and survives a reopen without further warnings.
    storage.user().insert_repository("a", Path::new("/r/a"), None).unwrap();
    drop(storage);
    let (storage, report) = open(&dir);
    assert!(report.warnings.is_empty());
    assert_eq!(storage.user().repositories().unwrap().len(), 1);
}

#[test]
fn corrupt_user_db_twice_gets_distinct_names() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    for _ in 0..2 {
        std::fs::write(data.join(USER_DB_FILE), vec![0xAB; 8192]).unwrap();
        let (_storage, report) = open(&dir);
        assert_eq!(report.warnings.len(), 1);
    }
    assert_eq!(corrupt_files(&data).len(), 2);
}

#[test]
fn user_db_damaged_pages_detected_by_quick_check() {
    let dir = TempDir::new().unwrap();
    let (storage, _) = open(&dir);
    for i in 0..200 {
        storage
            .user()
            .insert_repository(
                &format!("repo {i}"),
                &PathBuf::from(format!("/some/long/path/to/repository/{i:04}")),
                None,
            )
            .unwrap();
    }
    storage
        .user()
        .with_conn(|c| c.pragma_query(None, "wal_checkpoint", |_| Ok(())).unwrap());
    drop(storage);
    // Keep the valid header (first 100 bytes) but overwrite every following page with garbage.
    let path = dir.path().join("data").join(USER_DB_FILE);
    let mut bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() > 8192, "{}", bytes.len());
    for b in &mut bytes[4096..] {
        *b = 0x5A;
    }
    std::fs::write(&path, &bytes).unwrap();
    let (storage, report) = open(&dir);
    assert_eq!(report.warnings.len(), 1, "{report:?}");
    assert!(storage.user().repositories().unwrap().is_empty());
}

#[test]
fn user_db_from_newer_version_is_an_error_and_untouched() {
    let dir = TempDir::new().unwrap();
    let (storage, _) = open(&dir);
    storage.user().with_conn(|c| c.pragma_update(None, "user_version", 99)).unwrap();
    drop(storage);
    let err = Storage::open(&paths(&dir)).err().unwrap();
    assert!(matches!(err, GitYasError::Storage(ref m) if m.contains("newer")), "{err:?}");
    assert!(corrupt_files(&dir.path().join("data")).is_empty());
}

#[test]
fn garbage_cache_db_is_recreated_silently() {
    let dir = TempDir::new().unwrap();
    let cache = dir.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join(CACHE_DB_FILE), vec![0x42; 10_000]).unwrap();
    let (storage, report) = open(&dir);
    assert!(report.warnings.is_empty(), "{report:?}");
    storage
        .cache()
        .store_refresh(&repo("r"), &[commit(1, &[])], &[tip("refs/heads/main", 1)], &fp(1))
        .unwrap();
    assert!(storage.cache().snapshot(&repo("r")).unwrap().is_some());
}

#[test]
fn cache_version_mismatch_recreates() {
    let dir = TempDir::new().unwrap();
    let (storage, _) = open(&dir);
    storage
        .cache()
        .store_refresh(&repo("r"), &[commit(1, &[])], &[tip("refs/heads/main", 1)], &fp(1))
        .unwrap();
    storage.cache().with_writer(|c| c.pragma_update(None, "user_version", 7)).unwrap();
    drop(storage);
    let (storage, report) = open(&dir);
    assert!(report.warnings.is_empty());
    assert_eq!(storage.cache().snapshot(&repo("r")).unwrap(), None, "old cache content is gone");
    storage.cache().with_writer(|c| {
        let v: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(v, crate::schema::CACHE_VERSION);
    });
}

#[test]
fn unusable_cache_dir_falls_back_to_memory_with_warning() {
    let dir = TempDir::new().unwrap();
    let p = paths(&dir);
    std::fs::write(&p.cache_dir, b"a file where the cache dir should be").unwrap();
    let (storage, report) = Storage::open(&p).unwrap();
    assert_eq!(report.warnings.len(), 1);
    assert_eq!(storage.cache().path(), None);
    storage
        .cache()
        .store_refresh(&repo("r"), &[commit(1, &[])], &[tip("refs/heads/main", 1)], &fp(1))
        .unwrap();
    assert!(storage.cache().snapshot(&repo("r")).unwrap().is_some());
}

#[test]
fn reset_recreates_file_cache_and_clears_memory_cache() {
    let dir = TempDir::new().unwrap();
    let (file_storage, _) = open(&dir);
    let mem_storage = Storage::in_memory().unwrap();
    for storage in [&file_storage, &mem_storage] {
        let clone = storage.clone();
        storage
            .cache()
            .store_refresh(&repo("r"), &[commit(1, &[])], &[tip("refs/heads/main", 1)], &fp(1))
            .unwrap();
        storage.cache().reset().unwrap();
        assert_eq!(clone.cache().snapshot(&repo("r")).unwrap(), None);
        clone
            .cache()
            .store_refresh(&repo("r"), &[commit(2, &[])], &[tip("refs/heads/main", 2)], &fp(2))
            .unwrap();
        assert_eq!(storage.cache().load_reachable(&repo("r"), &[oid(2)]).unwrap().commits.len(), 1);
    }
}

// ---------------------------------------------------------------- concurrency

#[test]
fn in_memory_instances_are_isolated_and_reader_sees_writer() {
    let a = Storage::in_memory().unwrap();
    let b = Storage::in_memory().unwrap();
    a.cache()
        .store_refresh(&repo("r"), &[commit(1, &[])], &[tip("refs/heads/main", 1)], &fp(1))
        .unwrap();
    // Reader connection of `a` sees the writer's commit (shared memdb), `b` does not.
    assert!(a.cache().snapshot(&repo("r")).unwrap().is_some());
    assert!(b.cache().snapshot(&repo("r")).unwrap().is_none());
    a.user().insert_repository("x", Path::new("/x"), None).unwrap();
    assert!(b.user().repositories().unwrap().is_empty());
}

#[test]
fn usable_from_many_threads() {
    let storage = Storage::in_memory().unwrap();
    let handles: Vec<_> = (0..8u32)
        .map(|t| {
            let storage = storage.clone();
            std::thread::spawn(move || {
                for i in 0..25u32 {
                    let n = t * 1000 + i;
                    storage
                        .cache()
                        .store_refresh(&repo("r"), &[commit(n, &[])], &[tip(&format!("refs/heads/t{t}"), n)], &fp(1))
                        .unwrap();
                    storage.cache().load_reachable(&repo("r"), &[oid(n)]).unwrap();
                    storage
                        .user()
                        .insert_repository("x", &PathBuf::from(format!("/r/{n}")), None)
                        .unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(storage.user().repositories().unwrap().len(), 200);
    assert_eq!(count(&storage, "commits"), 200);
}

#[test]
fn wal_reader_not_blocked_by_open_write_transaction() {
    let dir = TempDir::new().unwrap();
    let (storage, _) = open(&dir);
    let cache = storage.cache();
    cache
        .store_refresh(&repo("r"), &[commit(1, &[])], &[tip("refs/heads/main", 1)], &fp(1))
        .unwrap();
    cache.with_writer(|w| {
        let tx = w.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
        tx.execute("UPDATE repo_cache SET refs_fingerprint = x'ff'", []).unwrap();
        // Reader connection still answers immediately, from the last committed state.
        let start = Instant::now();
        let (fingerprint, _) = cache.snapshot(&repo("r")).unwrap().unwrap();
        assert_eq!(fingerprint, fp(1));
        assert!(start.elapsed() < Duration::from_secs(1));
        tx.commit().unwrap();
    });
    assert_eq!(cache.snapshot(&repo("r")).unwrap().unwrap().0, RefsFingerprint(vec![0xff]));
}

#[test]
fn two_processes_contend_via_busy_timeout() {
    // Two independent `Storage::open` on the same files behave like two GitYas processes.
    let dir = TempDir::new().unwrap();
    let (a, _) = open(&dir);
    let (b, _) = open(&dir);

    // A holds the user DB write lock for a while; B's write waits (busy_timeout) instead of failing.
    let barrier = Arc::new(Barrier::new(2));
    let holder = {
        let a = a.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            a.user().with_conn(|c| {
                c.execute_batch("BEGIN IMMEDIATE").unwrap();
                c.execute("INSERT INTO repositories VALUES ('held', 'held', '/held', NULL, 100)", [])
                    .unwrap();
                barrier.wait();
                std::thread::sleep(Duration::from_millis(300));
                c.execute_batch("COMMIT").unwrap();
            });
        })
    };
    barrier.wait();
    let start = Instant::now();
    b.user().insert_repository("b", Path::new("/b"), None).unwrap();
    assert!(start.elapsed() >= Duration::from_millis(200), "B must have waited for A");
    holder.join().unwrap();
    assert_eq!(a.user().repositories().unwrap().len(), 2);

    // Concurrent cache refreshes of overlapping histories from both "processes" are harmless.
    let history: Vec<CommitRow> = (0..500)
        .map(|n| if n == 0 { commit(0, &[]) } else { commit(n, &[n - 1]) })
        .collect();
    let workers: Vec<_> = [a.clone(), b.clone()]
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let history = history.clone();
            std::thread::spawn(move || {
                for round in 0..10usize {
                    let end = 100 + (round * 40 + i * 20).min(399);
                    s.cache()
                        .store_refresh(
                            &repo("r"),
                            &history[..end],
                            &[tip("refs/heads/main", end as u32 - 1)],
                            &fp(round as u8),
                        )
                        .unwrap();
                    s.cache().load_reachable(&repo("r"), &[oid(end as u32 - 1)]).unwrap();
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    a.cache()
        .store_refresh(&repo("r"), &history, &[tip("refs/heads/main", 499)], &fp(9))
        .unwrap();
    let graph = b.cache().load_reachable(&repo("r"), &[oid(499)]).unwrap();
    assert!(graph.is_complete());
    assert_eq!(graph.commits.len(), 500);
}

// ---------------------------------------------------------------- cache DAO

/// 1 <- 2 <- 3 <- 5 (merge of 3 and 4), 2 <- 4; 6 is a side branch off 1.
fn sample() -> Vec<CommitRow> {
    vec![
        commit(1, &[]),
        commit(2, &[1]),
        commit(3, &[2]),
        commit(4, &[2]),
        commit(5, &[3, 4]),
        commit(6, &[1]),
    ]
}

#[test]
fn snapshot_round_trip() {
    let storage = Storage::in_memory().unwrap();
    let cache = storage.cache();
    assert_eq!(cache.snapshot(&repo("r")).unwrap(), None);
    let tips = vec![tip("refs/tags/v1", 2), tip("refs/heads/main", 5)];
    cache.store_refresh(&repo("r"), &sample(), &tips, &fp(1)).unwrap();
    let (fingerprint, stored) = cache.snapshot(&repo("r")).unwrap().unwrap();
    assert_eq!(fingerprint, fp(1));
    assert_eq!(stored, vec![tip("refs/heads/main", 5), tip("refs/tags/v1", 2)], "sorted by refname");

    // A later refresh replaces the snapshot (deleted refs disappear).
    cache.store_refresh(&repo("r"), &[], &[tip("refs/heads/side", 6)], &fp(2)).unwrap();
    assert_eq!(cache.snapshot(&repo("r")).unwrap(), Some((fp(2), vec![tip("refs/heads/side", 6)])));
    assert_eq!(cache.snapshot(&repo("other")).unwrap(), None);
}

#[test]
fn store_refresh_is_idempotent() {
    let storage = Storage::in_memory().unwrap();
    let cache = storage.cache();
    let tips = [tip("refs/heads/main", 5)];
    for _ in 0..3 {
        cache.store_refresh(&repo("r"), &sample(), &tips, &fp(1)).unwrap();
    }
    // Overlapping second batch.
    cache.store_refresh(&repo("r"), &sample()[2..], &tips, &fp(1)).unwrap();
    assert_eq!(count(&storage, "commits"), 6);
    assert_eq!(count(&storage, "commit_parents"), 6);
    assert_eq!(count(&storage, "ref_tips"), 1);
    assert_eq!(count(&storage, "repo_cache"), 1);
    // Same oids in another repo are separate rows.
    cache.store_refresh(&repo("r2"), &sample(), &tips, &fp(1)).unwrap();
    assert_eq!(count(&storage, "commits"), 12);
}

#[test]
fn store_refresh_is_atomic() {
    let storage = Storage::in_memory().unwrap();
    let cache = storage.cache();
    cache
        .store_refresh(&repo("r"), &sample(), &[tip("refs/heads/main", 5)], &fp(1))
        .unwrap();
    // Duplicate refname violates the ref_tips PK: nothing from this call may be committed.
    let err = cache
        .store_refresh(
            &repo("r"),
            &[commit(7, &[5])],
            &[tip("refs/heads/x", 7), tip("refs/heads/x", 7)],
            &fp(2),
        )
        .unwrap_err();
    assert!(matches!(err, GitYasError::Storage(_)));
    assert_eq!(count(&storage, "commits"), 6);
    assert_eq!(cache.snapshot(&repo("r")).unwrap().unwrap().0, fp(1));
}

#[test]
fn load_reachable_follows_parents() {
    let storage = Storage::in_memory().unwrap();
    let cache = storage.cache();
    cache
        .store_refresh(&repo("r"), &sample(), &[tip("refs/heads/main", 5)], &fp(1))
        .unwrap();

    let graph = cache.load_reachable(&repo("r"), &[oid(5)]).unwrap();
    assert!(graph.is_complete());
    assert_eq!(sorted_ids(&graph.commits), ids(&[1, 2, 3, 4, 5]));
    let merge = graph.commits.iter().find(|c| c.id == oid(5)).unwrap();
    assert_eq!(merge, &commit(5, &[3, 4]), "full row incl. parent order");

    let graph = cache.load_reachable(&repo("r"), &[oid(4), oid(6), oid(4)]).unwrap();
    assert_eq!(sorted_ids(&graph.commits), ids(&[1, 2, 4, 6]));
    assert_eq!(graph.commits.len(), 4, "each commit once");

    assert!(cache.load_reachable(&repo("r"), &[]).unwrap().commits.is_empty());
    assert!(cache.load_reachable(&repo("other"), &[oid(5)]).unwrap().commits.is_empty());
}

#[test]
fn load_reachable_reports_missing() {
    let storage = Storage::in_memory().unwrap();
    let cache = storage.cache();
    // 3's parent 2 was never cached; tip 9 is unknown.
    cache
        .store_refresh(
            &repo("r"),
            &[commit(3, &[2]), commit(4, &[3])],
            &[tip("refs/heads/main", 4)],
            &fp(1),
        )
        .unwrap();
    let graph = cache.load_reachable(&repo("r"), &[oid(4), oid(9)]).unwrap();
    assert!(!graph.is_complete());
    assert_eq!(graph.missing, ids(&[2, 9]));
    assert_eq!(sorted_ids(&graph.commits), ids(&[3, 4]));
}

#[test]
fn commit_files_round_trip() {
    let storage = Storage::in_memory().unwrap();
    let cache = storage.cache();
    let r = repo("r");
    assert_eq!(cache.commit_files(&r, &oid(1)).unwrap(), None);

    #[cfg_attr(not(unix), allow(unused_mut))] // the non-UTF-8 path row is Unix-only
    let mut files = vec![
        Delta {
            path: "src/main.rs".into(),
            old_path: None,
            change: DeltaChange::Modified,
        },
        Delta {
            path: "new name.txt".into(),
            old_path: Some("old name.txt".into()),
            change: DeltaChange::Renamed,
        },
        Delta {
            path: "gone".into(),
            old_path: None,
            change: DeltaChange::Typechange,
        },
    ];
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        files.push(Delta {
            path: PathBuf::from(std::ffi::OsStr::from_bytes(b"caf\xe9.txt")),
            old_path: None,
            change: DeltaChange::Added,
        });
    }
    cache.put_commit_files(&r, &oid(1), &files).unwrap();
    assert_eq!(cache.commit_files(&r, &oid(1)).unwrap(), Some(files.clone()));

    // Empty list is a cached value, distinct from "not cached".
    cache.put_commit_files(&r, &oid(2), &[]).unwrap();
    assert_eq!(cache.commit_files(&r, &oid(2)).unwrap(), Some(vec![]));

    // Put replaces.
    cache.put_commit_files(&r, &oid(1), &files[..1]).unwrap();
    assert_eq!(cache.commit_files(&r, &oid(1)).unwrap(), Some(files[..1].to_vec()));
    assert_eq!(cache.commit_files(&repo("other"), &oid(1)).unwrap(), None);
}

#[test]
fn every_delta_change_round_trips() {
    let storage = Storage::in_memory().unwrap();
    let all = [
        DeltaChange::Added,
        DeltaChange::Untracked,
        DeltaChange::Copied,
        DeltaChange::Deleted,
        DeltaChange::Renamed,
        DeltaChange::Modified,
        DeltaChange::Conflicted,
        DeltaChange::Typechange,
        DeltaChange::Ignored,
        DeltaChange::Unreadable,
    ];
    let files: Vec<Delta> = all
        .iter()
        .enumerate()
        .map(|(i, &change)| Delta {
            path: format!("f{i}").into(),
            old_path: None,
            change,
        })
        .collect();
    storage.cache().put_commit_files(&repo("r"), &oid(1), &files).unwrap();
    assert_eq!(storage.cache().commit_files(&repo("r"), &oid(1)).unwrap(), Some(files));
}

#[test]
fn purge_removes_only_that_repository() {
    let storage = Storage::in_memory().unwrap();
    let cache = storage.cache();
    for r in ["a", "b"] {
        cache
            .store_refresh(&repo(r), &sample(), &[tip("refs/heads/main", 5)], &fp(1))
            .unwrap();
        cache.put_commit_files(&repo(r), &oid(5), &[]).unwrap();
    }
    cache.purge_repository(&repo("a")).unwrap();
    assert_eq!(cache.snapshot(&repo("a")).unwrap(), None);
    assert!(cache.load_reachable(&repo("a"), &[oid(5)]).unwrap().commits.is_empty());
    assert_eq!(cache.commit_files(&repo("a"), &oid(5)).unwrap(), None);
    assert_eq!(cache.load_reachable(&repo("b"), &[oid(5)]).unwrap().commits.len(), 5);
    assert_eq!(cache.commit_files(&repo("b"), &oid(5)).unwrap(), Some(vec![]));
    for table in ["repo_cache", "ref_tips", "commits", "commit_parents", "commit_file_lists"] {
        let n: i64 = cache
            .with_reader(|c| c.query_row(&format!("SELECT count(*) FROM {table} WHERE repo_id = 'a'"), [], |r| r.get(0)))
            .unwrap();
        assert_eq!(n, 0, "{table}");
    }
    cache.purge_repository(&repo("never-cached")).unwrap();
}

#[test]
fn gc_by_age_marks_then_sweeps() {
    let storage = Storage::in_memory().unwrap();
    let cache = storage.cache();
    let r = repo("r");
    cache
        .store_refresh(&r, &sample(), &[tip("refs/heads/main", 5), tip("refs/heads/side", 6)], &fp(1))
        .unwrap();
    cache.put_commit_files(&r, &oid(6), &[]).unwrap();
    // Delete the side branch: 6 becomes unreachable.
    cache.store_refresh(&r, &[], &[tip("refs/heads/main", 5)], &fp(2)).unwrap();

    // Long max age: 6 is only marked.
    cache.gc(&r, 3600, u64::MAX).unwrap();
    assert_eq!(count(&storage, "commits"), 6);
    let marked: i64 = cache
        .with_reader(|c| c.query_row("SELECT count(*) FROM commits WHERE unreachable_since IS NOT NULL", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(marked, 1);

    // Branch restored before expiry: mark cleared, nothing deleted even with max age 0.
    cache
        .store_refresh(&r, &[], &[tip("refs/heads/main", 5), tip("refs/heads/side", 6)], &fp(1))
        .unwrap();
    cache.gc(&r, 0, u64::MAX).unwrap();
    assert_eq!(count(&storage, "commits"), 6);
    let marked: i64 = cache
        .with_reader(|c| c.query_row("SELECT count(*) FROM commits WHERE unreachable_since IS NOT NULL", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(marked, 0);

    // Deleted again, max age 0: swept with its parent links and file list.
    cache.store_refresh(&r, &[], &[tip("refs/heads/main", 5)], &fp(2)).unwrap();
    cache.gc(&r, 0, u64::MAX).unwrap();
    assert_eq!(count(&storage, "commits"), 5);
    assert_eq!(count(&storage, "commit_parents"), 5);
    assert_eq!(cache.commit_files(&r, &oid(6)).unwrap(), None);
    let graph = cache.load_reachable(&r, &[oid(5)]).unwrap();
    assert!(graph.is_complete());
    assert_eq!(graph.commits.len(), 5);
}

#[test]
fn gc_keeps_other_repositories_without_size_pressure() {
    let storage = Storage::in_memory().unwrap();
    let cache = storage.cache();
    cache.store_refresh(&repo("a"), &sample(), &[], &fp(1)).unwrap();
    cache
        .store_refresh(&repo("b"), &sample(), &[tip("refs/heads/main", 5)], &fp(1))
        .unwrap();
    cache.gc(&repo("b"), 0, u64::MAX).unwrap();
    assert_eq!(count(&storage, "commits"), 6 + 5);
    // gc of "a" (no tips) sweeps all of a.
    cache.gc(&repo("a"), 0, u64::MAX).unwrap();
    assert_eq!(count(&storage, "commits"), 5);
}

#[test]
fn gc_size_cap_drops_unreachable_and_other_repositories() {
    let dir = TempDir::new().unwrap();
    let (storage, _) = open(&dir);
    let cache = storage.cache();
    let history: Vec<CommitRow> = (0..3000)
        .map(|n| if n == 0 { commit(0, &[]) } else { commit(n, &[n - 1]) })
        .collect();
    cache
        .store_refresh(&repo("old"), &history, &[tip("refs/heads/main", 2999)], &fp(1))
        .unwrap();
    // "cur": 2000 reachable, plus 1000 unreachable on a deleted branch, plus a file list.
    let mut cur = history[..2000].to_vec();
    cur.extend((5000..6000).map(|n| commit(n, &[n - 1])).map(|mut c| {
        if c.id == oid(5000) {
            c.parents = vec![oid(10)];
        }
        c
    }));
    cache
        .store_refresh(&repo("cur"), &cur, &[tip("refs/heads/main", 1999)], &fp(1))
        .unwrap();
    cache.put_commit_files(&repo("cur"), &oid(5), &[]).unwrap();

    // Generous age, tiny size cap: everything except cur's reachable commits goes.
    cache.gc(&repo("cur"), 3600, 1).unwrap();
    assert_eq!(cache.snapshot(&repo("old")).unwrap(), None);
    assert!(cache.load_reachable(&repo("old"), &[oid(2999)]).unwrap().commits.is_empty());
    let graph = cache.load_reachable(&repo("cur"), &[oid(1999)]).unwrap();
    assert!(graph.is_complete());
    assert_eq!(graph.commits.len(), 2000);
    assert_eq!(count(&storage, "commits"), 2000);
    assert_eq!(cache.commit_files(&repo("cur"), &oid(5)).unwrap(), None);
    // Free pages were handed back.
    let free: i64 = cache
        .with_writer(|c| c.pragma_query_value(None, "freelist_count", |r| r.get(0)))
        .unwrap();
    assert_eq!(free, 0);
}

/// Bulk insert + load of a 100k-commit history (linear with a merge every 50 commits). Prints timings;
/// run with `--nocapture` to see them. Asserts only generous bounds so it is stable in debug builds.
#[test]
fn bulk_100k_insert_and_load_reachable() {
    const N: u32 = 100_000;
    let dir = TempDir::new().unwrap();
    let (storage, _) = open(&dir);
    let cache = storage.cache();
    let commits: Vec<CommitRow> = (0..N)
        .map(|n| match n {
            0 => commit(0, &[]),
            n if n % 50 == 0 && n >= 2 => commit(n, &[n - 1, n - 2]),
            n => commit(n, &[n - 1]),
        })
        .collect();
    let tips = [tip("refs/heads/main", N - 1)];

    let start = Instant::now();
    cache.store_refresh(&repo("big"), &commits, &tips, &fp(1)).unwrap();
    let insert = start.elapsed();

    let start = Instant::now();
    let graph = cache.load_reachable(&repo("big"), &[oid(N - 1)]).unwrap();
    let load = start.elapsed();
    assert!(graph.is_complete());
    assert_eq!(graph.commits.len(), N as usize);

    let start = Instant::now();
    cache.store_refresh(&repo("big"), &commits, &tips, &fp(1)).unwrap();
    let reinsert = start.elapsed();

    let start = Instant::now();
    cache.gc(&repo("big"), 0, u64::MAX).unwrap();
    let gc = start.elapsed();
    assert_eq!(count(&storage, "commits"), i64::from(N));

    println!("100k commits: insert {insert:?}, load_reachable {load:?}, idempotent re-insert {reinsert:?}, gc {gc:?}");
    assert!(load < Duration::from_secs(20), "load_reachable took {load:?}");
}
