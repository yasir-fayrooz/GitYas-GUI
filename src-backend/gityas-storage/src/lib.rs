//! SQLite storage (WP2, §3): `gityas.db` (user data, data dir) and `cache.db` (disposable, cache dir).
//! Never touches `git2`; secrets never stored here.
//!
//! Both DAOs are `Clone + Send + Sync` handles over `Arc<Mutex<Connection>>`; calls are synchronous and meant to
//! run on `spawn_blocking`. Schema and migrations: `schema.rs`.

mod cache;
mod db;
mod schema;
mod user;

#[cfg(test)]
mod tests;

use std::path::PathBuf;

use gityas_core::{GitYasError, Result};

pub use cache::{CacheDb, ReachableGraph};
pub use user::UserDb;

use user::UserOpenError;

pub const USER_DB_FILE: &str = "gityas.db";
pub const CACHE_DB_FILE: &str = "cache.db";

/// Directories the databases live in. The engine builds this from `gityas_platform::Paths`
/// (storage does not depend on platform, keeping the dependency rule `core <- storage`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoragePaths {
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
}

/// Both databases. Cheap to clone; share it between tasks.
#[derive(Clone)]
pub struct Storage {
    user: UserDb,
    cache: CacheDb,
}

/// Outcome of opening storage: non-fatal recoveries the engine reports as `AppEvent::Warning`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenReport {
    pub warnings: Vec<String>,
}

impl Storage {
    /// Opens (creating/migrating) both DBs; creates the directories if needed.
    ///
    /// * User DB: if the file is not a valid database (or fails `quick_check`), it and its `-wal`/`-shm` files are
    ///   renamed `gityas.db.corrupt-<unix-ts>[-n]`, an empty DB is created and a warning is added to the report.
    ///   Other failures (permissions, busy, a schema from a newer GitYas) are returned as errors and never touch
    ///   the file.
    /// * Cache DB: on a schema-version mismatch or any open error the file is deleted and recreated silently. If
    ///   even that fails, an in-memory cache is used for this session and a warning is reported.
    pub fn open(paths: &StoragePaths) -> Result<(Storage, OpenReport)> {
        let mut report = OpenReport::default();
        std::fs::create_dir_all(&paths.data_dir)
            .map_err(|err| GitYasError::Io(format!("cannot create {}: {err}", paths.data_dir.display())))?;

        let user_path = paths.data_dir.join(USER_DB_FILE);
        let user = match UserDb::open_file(&user_path) {
            Ok(user) => user,
            Err(UserOpenError::Fatal(err)) => return Err(err),
            Err(UserOpenError::Corrupt(reason)) => {
                tracing::error!(path = %user_path.display(), %reason, "user database is corrupt; resetting it");
                let moved = db::quarantine_db_files(&user_path, "corrupt")?;
                report.warnings.push(format!(
                    "The GitYas database was damaged and has been reset (repositories and settings were lost). \
                     The damaged file was kept as {}.",
                    moved.display()
                ));
                match UserDb::open_file(&user_path) {
                    Ok(user) => user,
                    Err(UserOpenError::Fatal(err)) => return Err(err),
                    Err(UserOpenError::Corrupt(reason)) => {
                        return Err(GitYasError::Storage(format!("cannot create a fresh user database: {reason}")));
                    }
                }
            }
        };

        let cache_path = paths.cache_dir.join(CACHE_DB_FILE);
        let cache = std::fs::create_dir_all(&paths.cache_dir)
            .map_err(|err| GitYasError::Io(format!("cannot create {}: {err}", paths.cache_dir.display())))
            .and_then(|()| CacheDb::open_file(&cache_path));
        let cache = match cache {
            Ok(cache) => cache,
            Err(err) => {
                tracing::error!(path = %cache_path.display(), %err, "cannot open the cache database; using memory");
                report.warnings.push(format!(
                    "The history cache could not be opened ({err}); GitYas will run without a persistent cache."
                ));
                CacheDb::open_in_memory()?
            }
        };
        Ok((Storage { user, cache }, report))
    }

    /// Both DBs in memory (tests). The cache uses a uniquely named memdb database so its writer and reader
    /// connections share it; separate `in_memory()` calls never share data.
    pub fn in_memory() -> Result<Storage> {
        Ok(Storage {
            user: UserDb::open_in_memory()?,
            cache: CacheDb::open_in_memory()?,
        })
    }

    pub fn user(&self) -> &UserDb {
        &self.user
    }

    pub fn cache(&self) -> &CacheDb {
        &self.cache
    }
}
