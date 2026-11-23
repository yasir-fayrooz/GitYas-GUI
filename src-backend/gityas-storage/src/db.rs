//! Connection plumbing shared by both databases: pragmas, error mapping, corruption classification,
//! file helpers and the clock.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gityas_core::{GitYasError, Result};
use rusqlite::{Connection, ErrorCode, OpenFlags};

/// `busy_timeout` for every connection: waits for another connection/process instead of failing.
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

/// Maps a SQLite error to the crate-boundary error type (orphan rule forbids a `From` impl).
pub(crate) fn db(err: rusqlite::Error) -> GitYasError {
    GitYasError::Storage(err.to_string())
}

pub(crate) fn migration(err: rusqlite_migration::Error) -> GitYasError {
    GitYasError::Storage(format!("migration failed: {err}"))
}

/// True for errors meaning "this file is not a usable SQLite database" (as opposed to busy/IO/permission errors,
/// which must never trigger the destructive recovery path).
pub(crate) fn is_corruption(err: &rusqlite::Error) -> bool {
    matches!(err.sqlite_error_code(), Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt))
}

pub(crate) fn migration_is_corruption(err: &rusqlite_migration::Error) -> bool {
    match err {
        rusqlite_migration::Error::RusqliteError { err, .. } => is_corruption(err),
        _ => false,
    }
}

/// Locks a connection. A poisoned mutex only means another thread panicked while holding it; any transaction it
/// had open was rolled back when the `Transaction` guard dropped during unwinding, so the connection is still
/// consistent and keeps being used (otherwise one panic would disable storage for the rest of the session).
pub(crate) fn lock(conn: &Mutex<Connection>) -> MutexGuard<'_, Connection> {
    conn.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn unix_now() -> Result<i64> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| GitYasError::Storage("system clock is set before 1970".into()))?
        .as_secs();
    i64::try_from(secs).map_err(|_| GitYasError::Storage("system clock out of range".into()))
}

pub(crate) fn file_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

/// Flags for a named in-memory database shared between connections of this process
/// (`file:/<name>?vfs=memdb`). Unlike `cache=shared`, memdb uses ordinary file locking, so `busy_timeout`
/// applies instead of immediate `SQLITE_LOCKED` errors.
pub(crate) fn memdb_flags() -> OpenFlags {
    file_flags() | OpenFlags::SQLITE_OPEN_URI
}

pub(crate) fn memdb_uri(name: &str) -> String {
    format!("file:/{name}?vfs=memdb")
}

/// Common pragmas. `wal`: request WAL (file DBs only; in-memory DBs have no WAL).
pub(crate) fn configure(conn: &Connection, wal: bool) -> rusqlite::Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    if wal {
        let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            // Not fatal (e.g. a filesystem without shared memory): SQLite falls back to a rollback journal and
            // busy_timeout still serialises access. Logged so it is visible.
            tracing::warn!(%mode, "SQLite refused WAL journal mode");
        }
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn journal_mode(conn: &Connection) -> rusqlite::Result<String> {
    conn.pragma_query_value(None, "journal_mode", |row| row.get(0))
}

/// `PRAGMA quick_check`: `Ok(None)` if healthy, otherwise the first problem reported.
pub(crate) fn quick_check(conn: &Connection) -> rusqlite::Result<Option<String>> {
    let first: String = conn.pragma_query_value(None, "quick_check", |row| row.get(0))?;
    Ok(if first == "ok" { None } else { Some(first) })
}

/// Checks the 16-byte SQLite header of an existing, non-empty file. `Ok(None)`: missing, empty or plausible.
pub(crate) fn header_problem(path: &Path) -> Result<Option<String>> {
    use std::io::Read;
    const MAGIC: &[u8; 16] = b"SQLite format 3\0";
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(GitYasError::Io(format!("cannot read {}: {err}", path.display()))),
    };
    let mut header = Vec::with_capacity(16);
    file.by_ref()
        .take(16)
        .read_to_end(&mut header)
        .map_err(|err| GitYasError::Io(format!("cannot read {}: {err}", path.display())))?;
    if header.is_empty() || header.as_slice() == MAGIC {
        Ok(None)
    } else {
        Ok(Some("file is not an SQLite database (bad header)".into()))
    }
}

/// `<db>-wal` / `<db>-shm` sidecar of a database file.
pub(crate) fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

pub(crate) const SIDECARS: [&str; 3] = ["-wal", "-shm", "-journal"];

/// Deletes a database file and its sidecars. Missing files are fine; other errors are returned.
pub(crate) fn remove_db_files(path: &Path) -> Result<()> {
    let mut targets = vec![path.to_path_buf()];
    targets.extend(SIDECARS.iter().map(|s| sidecar(path, s)));
    for target in targets {
        match std::fs::remove_file(&target) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(GitYasError::Io(format!("cannot delete {}: {err}", target.display()))),
        }
    }
    Ok(())
}

/// Moves a database file and its sidecars aside as `<db>.<tag>-<ts>[-n]` (sidecars keep their `-wal`/`-shm`
/// suffix after the new name, so SQLite still pairs them if the copy is opened). Returns the new main path.
pub(crate) fn quarantine_db_files(path: &Path, tag: &str) -> Result<PathBuf> {
    let ts = unix_now()?;
    let mut n = 0u32;
    let target = loop {
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".{tag}-{ts}"));
        if n > 0 {
            name.push(format!("-{n}"));
        }
        let candidate = PathBuf::from(name);
        if !candidate.exists() {
            break candidate;
        }
        n += 1;
    };
    std::fs::rename(path, &target).map_err(|err| GitYasError::Io(format!("cannot move {} aside: {err}", path.display())))?;
    for suffix in SIDECARS {
        let from = sidecar(path, suffix);
        if from.exists() {
            let to = sidecar(&target, suffix);
            std::fs::rename(&from, &to).map_err(|err| GitYasError::Io(format!("cannot move {} aside: {err}", from.display())))?;
        }
    }
    Ok(target)
}
