use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gityas_core::models::{CommitSignature, OpenTabs, Repository, Settings};
use gityas_core::{GitYasError, Provider, RepoId, Result};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Map, Value};

use crate::db::{self, db, lock};
use crate::schema;

const REPO_COLUMNS: &str = "id, name, path, provider, sort_order";
const SIGNATURE_KEY: &str = "commit_signature";

/// User DB DAO: repositories, settings, open tabs. One connection (tiny load). Last writer wins between processes.
#[derive(Clone)]
pub struct UserDb {
    conn: Arc<Mutex<Connection>>,
    path: Option<PathBuf>,
}

/// Why opening the user DB file failed.
pub(crate) enum UserOpenError {
    /// The file is not a usable database: quarantine it and start fresh.
    Corrupt(String),
    /// Anything else (busy, permissions, newer schema): never destroy the file, surface the error.
    Fatal(GitYasError),
}

impl UserDb {
    pub(crate) fn open_file(path: &Path) -> std::result::Result<UserDb, UserOpenError> {
        let classify = |err: rusqlite::Error| {
            if db::is_corruption(&err) {
                UserOpenError::Corrupt(err.to_string())
            } else {
                UserOpenError::Fatal(db(err))
            }
        };
        // Check the header before SQLite touches the file, so a damaged file and its sidecars are quarantined
        // exactly as found (SQLite may otherwise discard a WAL it cannot pair with the main file).
        if let Some(problem) = db::header_problem(path).map_err(UserOpenError::Fatal)? {
            return Err(UserOpenError::Corrupt(problem));
        }
        let mut conn = Connection::open_with_flags(path, db::file_flags()).map_err(classify)?;
        db::configure(&conn, true).map_err(classify)?;
        if let Some(problem) = db::quick_check(&conn).map_err(classify)? {
            return Err(UserOpenError::Corrupt(problem));
        }
        Self::prepare(&mut conn).map_err(|err| match err {
            PrepareError::Corrupt(msg) => UserOpenError::Corrupt(msg),
            PrepareError::Fatal(err) => UserOpenError::Fatal(err),
        })?;
        Ok(UserDb {
            conn: Arc::new(Mutex::new(conn)),
            path: Some(path.to_path_buf()),
        })
    }

    pub(crate) fn open_in_memory() -> Result<UserDb> {
        let mut conn = Connection::open_in_memory().map_err(db)?;
        db::configure(&conn, false).map_err(db)?;
        Self::prepare(&mut conn).map_err(|err| match err {
            PrepareError::Corrupt(msg) => GitYasError::Storage(msg),
            PrepareError::Fatal(err) => err,
        })?;
        Ok(UserDb {
            conn: Arc::new(Mutex::new(conn)),
            path: None,
        })
    }

    fn prepare(conn: &mut Connection) -> std::result::Result<(), PrepareError> {
        conn.pragma_update(None, "foreign_keys", true).map_err(PrepareError::from_sqlite)?;
        let migrations = schema::user_migrations();
        let version = migrations.current_version(conn).map_err(PrepareError::from_migration)?;
        if let rusqlite_migration::SchemaVersion::Outside(v) = version {
            return Err(PrepareError::Fatal(GitYasError::Storage(format!(
                "the user database has schema version {v}, written by a newer GitYas; update GitYas to open it"
            ))));
        }
        migrations.to_latest(conn).map_err(PrepareError::from_migration)?;
        Ok(())
    }

    /// Database file, `None` for in-memory storage.
    pub fn path(&self) -> Option<PathBuf> {
        self.path.clone()
    }

    pub fn repositories(&self) -> Result<Vec<Repository>> {
        let conn = lock(&self.conn);
        let mut stmt = conn
            .prepare_cached(&format!("SELECT {REPO_COLUMNS} FROM repositories ORDER BY sort_order, id"))
            .map_err(db)?;
        let rows = stmt.query_map([], RawRepo::from_row).map_err(db)?;
        rows.map(|row| row.map_err(db)?.into_repository()).collect()
    }

    pub fn repository(&self, id: &RepoId) -> Result<Option<Repository>> {
        let conn = lock(&self.conn);
        conn.query_row(
            &format!("SELECT {REPO_COLUMNS} FROM repositories WHERE id = ?1"),
            [id],
            RawRepo::from_row,
        )
        .optional()
        .map_err(db)?
        .map(RawRepo::into_repository)
        .transpose()
    }

    /// Inserts at the end of the sort order with a fresh id. Fails with `InvalidInput` if the path is already
    /// registered. Paths compare case-insensitively (ASCII only, SQLite `NOCASE`) after lexical normalisation
    /// (trailing separators and `.` components removed); the engine is expected to canonicalise beforehand.
    /// Non-UTF-8 paths are rejected (`InvalidInput`).
    pub fn insert_repository(&self, name: &str, path: &Path, provider: Option<Provider>) -> Result<Repository> {
        let name = name.trim();
        if name.is_empty() {
            return Err(GitYasError::InvalidInput("repository name is empty".into()));
        }
        let path = normalize_path(path)?;
        let mut conn = lock(&self.conn);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        let existing: Option<String> = tx
            .query_row("SELECT path FROM repositories WHERE path = ?1", [&path], |row| row.get(0))
            .optional()
            .map_err(db)?;
        if let Some(existing) = existing {
            return Err(GitYasError::AlreadyExists(format!("repository {existing}")));
        }
        let sort_order: i64 = tx
            .query_row("SELECT COALESCE(MAX(sort_order) + 1, 0) FROM repositories", [], |row| row.get(0))
            .map_err(db)?;
        let id: RepoId = nanoid::nanoid!();
        tx.execute(
            "INSERT INTO repositories (id, name, path, provider, sort_order) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, name, path, provider.map(Provider::key), sort_order],
        )
        .map_err(db)?;
        tx.commit().map_err(db)?;
        Ok(Repository {
            id,
            name: name.to_owned(),
            path: PathBuf::from(path),
            provider,
            sort_order,
        })
    }

    pub fn update_repository_provider(&self, id: &RepoId, provider: Option<Provider>) -> Result<()> {
        let conn = lock(&self.conn);
        let changed = conn
            .execute(
                "UPDATE repositories SET provider = ?2 WHERE id = ?1",
                params![id, provider.map(Provider::key)],
            )
            .map_err(db)?;
        if changed == 0 {
            return Err(not_found(id));
        }
        Ok(())
    }

    /// `ordered` must be a permutation of the registered ids (`InvalidInput` otherwise; nothing changes).
    pub fn reorder_repositories(&self, ordered: &[RepoId]) -> Result<()> {
        let mut conn = lock(&self.conn);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        let registered: HashSet<String> = {
            let mut stmt = tx.prepare_cached("SELECT id FROM repositories").map_err(db)?;
            let ids = stmt.query_map([], |row| row.get(0)).map_err(db)?;
            ids.collect::<rusqlite::Result<_>>().map_err(db)?
        };
        let given: HashSet<&str> = ordered.iter().map(String::as_str).collect();
        if given.len() != ordered.len() {
            return Err(GitYasError::InvalidInput("repository order contains duplicates".into()));
        }
        if ordered.len() != registered.len() || !ordered.iter().all(|id| registered.contains(id)) {
            return Err(GitYasError::InvalidInput(
                "repository order must list every registered repository exactly once".into(),
            ));
        }
        {
            let mut stmt = tx
                .prepare_cached("UPDATE repositories SET sort_order = ?2 WHERE id = ?1")
                .map_err(db)?;
            for (position, id) in ordered.iter().enumerate() {
                let position = i64::try_from(position).map_err(|_| GitYasError::InvalidInput("too many repositories".into()))?;
                stmt.execute(params![id, position]).map_err(db)?;
            }
        }
        tx.commit().map_err(db)
    }

    /// Removes the repository; its tab is closed and, if it was active, the selection is cleared (same
    /// transaction, via foreign keys). `NotFound` if the id is unknown. The engine purges its cache separately.
    pub fn delete_repository(&self, id: &RepoId) -> Result<()> {
        let conn = lock(&self.conn);
        let changed = conn.execute("DELETE FROM repositories WHERE id = ?1", [id]).map_err(db)?;
        if changed == 0 {
            return Err(not_found(id));
        }
        Ok(())
    }

    /// Loads settings. Keys that `Settings` does not know are ignored (kept in the table for newer versions).
    /// A stored value that no longer deserialises falls back to that field's default and is logged; the
    /// stored value is left untouched until the next save.
    pub fn settings(&self) -> Result<Settings> {
        let stored: Vec<(String, String)> = {
            let conn = lock(&self.conn);
            let mut stmt = conn.prepare_cached("SELECT key, value FROM settings").map_err(db)?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?))).map_err(db)?;
            rows.collect::<rusqlite::Result<_>>().map_err(db)?
        };
        let mut object = settings_object(&Settings::default())?;
        for (key, raw) in stored {
            let Some(default) = object.get(&key).cloned() else {
                tracing::debug!(%key, "ignoring unknown settings key");
                continue;
            };
            let parsed: Value = match serde_json::from_str(&raw) {
                Ok(value) => value,
                Err(err) => {
                    tracing::warn!(%key, %err, "stored setting is not valid JSON; using the default");
                    continue;
                }
            };
            object.insert(key.clone(), parsed);
            if let Err(err) = serde_json::from_value::<Settings>(Value::Object(object.clone())) {
                tracing::warn!(%key, %err, "stored setting has an unexpected shape; using the default");
                object.insert(key, default);
            }
        }
        serde_json::from_value(Value::Object(object)).map_err(|err| GitYasError::Storage(format!("cannot decode settings: {err}")))
    }

    /// Writes every `Settings` field (one row per field) in one transaction.
    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        if let Some(signature) = &settings.commit_signature {
            validate_signature(signature)?;
        }
        let object = settings_object(settings)?;
        let mut conn = lock(&self.conn);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        {
            let mut stmt = tx
                .prepare_cached("INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
                .map_err(db)?;
            for (key, value) in &object {
                stmt.execute(params![key, value.to_string()]).map_err(db)?;
            }
        }
        tx.commit().map_err(db)
    }

    /// Stores the commit signature. Rejects a blank name or email (`InvalidInput`, bug-log §8.1 #5).
    pub fn set_commit_signature(&self, signature: &CommitSignature) -> Result<()> {
        validate_signature(signature)?;
        let value = serde_json::to_string(signature).map_err(|err| GitYasError::Storage(format!("cannot encode signature: {err}")))?;
        let conn = lock(&self.conn);
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SIGNATURE_KEY, value],
        )
        .map_err(db)?;
        Ok(())
    }

    pub fn open_tabs(&self) -> Result<OpenTabs> {
        let conn = lock(&self.conn);
        let open = {
            let mut stmt = conn.prepare_cached("SELECT repo_id FROM open_tabs ORDER BY position").map_err(db)?;
            let ids = stmt.query_map([], |row| row.get(0)).map_err(db)?;
            ids.collect::<rusqlite::Result<Vec<RepoId>>>().map_err(db)?
        };
        let active = conn
            .query_row("SELECT repo_id FROM active_repository WHERE id = 1", [], |row| {
                row.get::<_, Option<RepoId>>(0)
            })
            .optional()
            .map_err(db)?
            .flatten();
        Ok(OpenTabs { open, active })
    }

    /// Replaces the open tabs and active repository. Every id must be registered and `open` must not contain
    /// duplicates (`InvalidInput`; nothing changes). `active` need not be in `open`.
    pub fn set_open_tabs(&self, tabs: &OpenTabs) -> Result<()> {
        let unique: HashSet<&str> = tabs.open.iter().map(String::as_str).collect();
        if unique.len() != tabs.open.len() {
            return Err(GitYasError::InvalidInput("open tabs contain duplicates".into()));
        }
        let mut conn = lock(&self.conn);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        {
            let mut exists = tx.prepare_cached("SELECT 1 FROM repositories WHERE id = ?1").map_err(db)?;
            for id in tabs.open.iter().chain(tabs.active.iter()) {
                if !exists.exists([id]).map_err(db)? {
                    return Err(GitYasError::InvalidInput(format!("unknown repository id in open tabs: {id}")));
                }
            }
        }
        tx.execute("DELETE FROM open_tabs", []).map_err(db)?;
        {
            let mut insert = tx
                .prepare_cached("INSERT INTO open_tabs (repo_id, position) VALUES (?1, ?2)")
                .map_err(db)?;
            for (position, id) in tabs.open.iter().enumerate() {
                let position = i64::try_from(position).map_err(|_| GitYasError::InvalidInput("too many tabs".into()))?;
                insert.execute(params![id, position]).map_err(db)?;
            }
        }
        tx.execute(
            "INSERT INTO active_repository (id, repo_id) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET repo_id = excluded.repo_id",
            [&tabs.active],
        )
        .map_err(db)?;
        tx.commit().map_err(db)
    }

    #[cfg(test)]
    pub(crate) fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        f(&lock(&self.conn))
    }
}

enum PrepareError {
    Corrupt(String),
    Fatal(GitYasError),
}

impl PrepareError {
    fn from_sqlite(err: rusqlite::Error) -> Self {
        if db::is_corruption(&err) {
            Self::Corrupt(err.to_string())
        } else {
            Self::Fatal(db(err))
        }
    }

    fn from_migration(err: rusqlite_migration::Error) -> Self {
        if db::migration_is_corruption(&err) {
            Self::Corrupt(err.to_string())
        } else {
            Self::Fatal(db::migration(err))
        }
    }
}

struct RawRepo {
    id: String,
    name: String,
    path: String,
    provider: Option<String>,
    sort_order: i64,
}

impl RawRepo {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            path: row.get(2)?,
            provider: row.get(3)?,
            sort_order: row.get(4)?,
        })
    }

    fn into_repository(self) -> Result<Repository> {
        let provider = match self.provider {
            None => None,
            Some(key) => Some(
                Provider::from_key(&key)
                    .ok_or_else(|| GitYasError::Storage(format!("unknown provider {key:?} for repository {}", self.id)))?,
            ),
        };
        Ok(Repository {
            id: self.id,
            name: self.name,
            path: PathBuf::from(self.path),
            provider,
            sort_order: self.sort_order,
        })
    }
}

fn not_found(id: &RepoId) -> GitYasError {
    GitYasError::NotFound(format!("repository {id}"))
}

fn validate_signature(signature: &CommitSignature) -> Result<()> {
    if signature.is_valid() {
        Ok(())
    } else {
        Err(GitYasError::InvalidInput("commit signature needs a name and an email".into()))
    }
}

fn settings_object(settings: &Settings) -> Result<Map<String, Value>> {
    match serde_json::to_value(settings) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(other) => Err(GitYasError::Storage(format!("settings serialised to a non-object: {other}"))),
        Err(err) => Err(GitYasError::Storage(format!("cannot encode settings: {err}"))),
    }
}

/// Lexical normalisation (no filesystem access): drops trailing separators and `.` components.
fn normalize_path(path: &Path) -> Result<String> {
    let normalized: PathBuf = path.components().collect();
    if normalized.as_os_str().is_empty() {
        return Err(GitYasError::InvalidInput("repository path is empty".into()));
    }
    normalized
        .into_os_string()
        .into_string()
        .map_err(|raw| GitYasError::InvalidInput(format!("repository path is not valid UTF-8: {}", PathBuf::from(raw).display())))
}

#[cfg(test)]
mod tests {
    use gityas_core::models::{DiffViewMode, Theme};

    use super::*;

    fn db() -> UserDb {
        UserDb::open_in_memory().unwrap()
    }

    #[test]
    fn empty_db_has_defaults() {
        let db = db();
        assert!(db.repositories().unwrap().is_empty());
        assert_eq!(db.settings().unwrap(), Settings::default());
        assert_eq!(db.open_tabs().unwrap(), OpenTabs::default());
    }

    #[test]
    fn repository_crud() {
        let db = db();
        let a = db.insert_repository("a", Path::new("/r/a"), Some(Provider::Github)).unwrap();
        let b = db.insert_repository(" b ", Path::new("/r/b/"), None).unwrap();
        assert_eq!(a.sort_order, 0);
        assert_eq!(b.sort_order, 1);
        assert_eq!(b.name, "b");
        assert_eq!(b.path, PathBuf::from("/r/b"));
        assert_eq!(db.repositories().unwrap(), vec![a.clone(), b.clone()]);
        assert_eq!(db.repository(&a.id).unwrap(), Some(a.clone()));
        assert_eq!(db.repository(&"nope".to_owned()).unwrap(), None);

        db.update_repository_provider(&b.id, Some(Provider::AzureDevops)).unwrap();
        assert_eq!(db.repository(&b.id).unwrap().unwrap().provider, Some(Provider::AzureDevops));
        db.update_repository_provider(&b.id, None).unwrap();
        assert_eq!(db.repository(&b.id).unwrap().unwrap().provider, None);
        assert!(matches!(
            db.update_repository_provider(&"x".into(), None),
            Err(GitYasError::NotFound(_))
        ));

        db.delete_repository(&a.id).unwrap();
        assert_eq!(db.repositories().unwrap(), vec![b.clone()]);
        assert!(matches!(db.delete_repository(&a.id), Err(GitYasError::NotFound(_))));

        // New inserts go after the current maximum.
        let c = db.insert_repository("c", Path::new("/r/c"), None).unwrap();
        assert_eq!(c.sort_order, 2);
    }

    #[test]
    fn duplicate_path_is_case_insensitive() {
        let db = db();
        db.insert_repository("a", Path::new("/Users/Me/Repo"), None).unwrap();
        for dup in ["/users/me/repo", "/USERS/ME/REPO/", "/Users/Me/./Repo"] {
            let err = db.insert_repository("dup", Path::new(dup), None).unwrap_err();
            assert!(matches!(err, GitYasError::AlreadyExists(_)), "{dup}: {err:?}");
        }
        assert_eq!(db.repositories().unwrap().len(), 1);
        // The UNIQUE NOCASE constraint itself also holds (defence in depth). Upper-cased stored path, since the
        // separators are platform-specific.
        let stored = db.repositories().unwrap()[0].path.to_string_lossy().to_uppercase();
        let raw = db.with_conn(|c| {
            c.execute(
                "INSERT INTO repositories (id, name, path, provider, sort_order) VALUES ('x', 'x', ?1, NULL, 9)",
                [&stored],
            )
        });
        assert!(raw.is_err());
    }

    #[test]
    fn rejects_bad_input() {
        let db = db();
        assert!(matches!(
            db.insert_repository("  ", Path::new("/a"), None),
            Err(GitYasError::InvalidInput(_))
        ));
        assert!(matches!(
            db.insert_repository("a", Path::new(""), None),
            Err(GitYasError::InvalidInput(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_path() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let db = db();
        let path = Path::new(OsStr::from_bytes(b"/r/\xff"));
        assert!(matches!(db.insert_repository("a", path, None), Err(GitYasError::InvalidInput(_))));
    }

    #[test]
    fn reorder_must_be_a_permutation() {
        let db = db();
        let ids: Vec<RepoId> = ["a", "b", "c"]
            .iter()
            .map(|n| db.insert_repository(n, &Path::new("/r").join(n), None).unwrap().id)
            .collect();
        let before = db.repositories().unwrap();
        let bad: [Vec<RepoId>; 5] = [
            vec![],
            vec![ids[0].clone(), ids[1].clone()],
            vec![ids[0].clone(), ids[1].clone(), ids[1].clone()],
            vec![ids[0].clone(), ids[1].clone(), ids[2].clone(), "x".into()],
            vec![ids[0].clone(), ids[1].clone(), "x".into()],
        ];
        for order in bad {
            assert!(
                matches!(db.reorder_repositories(&order), Err(GitYasError::InvalidInput(_))),
                "{order:?}"
            );
            assert_eq!(db.repositories().unwrap(), before);
        }
        let new = vec![ids[2].clone(), ids[0].clone(), ids[1].clone()];
        db.reorder_repositories(&new).unwrap();
        let after: Vec<RepoId> = db.repositories().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(after, new);
    }

    #[test]
    fn settings_round_trip() {
        let db = db();
        let mut settings = Settings {
            commit_signature: Some(CommitSignature {
                name: "Me".into(),
                email: "me@example.com".into(),
            }),
            theme: Theme::Dark,
            diff_view_mode: DiffViewMode::Split,
            ..Settings::default()
        };
        settings.graph.show_tags = false;
        db.save_settings(&settings).unwrap();
        assert_eq!(db.settings().unwrap(), settings);

        let sig = CommitSignature {
            name: "Other".into(),
            email: "o@example.com".into(),
        };
        db.set_commit_signature(&sig).unwrap();
        assert_eq!(db.settings().unwrap().commit_signature, Some(sig));
        assert_eq!(db.settings().unwrap().theme, Theme::Dark);

        let blank = CommitSignature {
            name: " ".into(),
            email: "x@y".into(),
        };
        assert!(matches!(db.set_commit_signature(&blank), Err(GitYasError::InvalidInput(_))));
    }

    #[test]
    fn unknown_and_malformed_settings_are_ignored() {
        let db = db();
        db.with_conn(|c| {
            c.execute_batch(
                r#"INSERT INTO settings (key, value) VALUES
                    ('from_the_future', '{"x": 1}'),
                    ('theme', '"dark"'),
                    ('diff_view_mode', '"no-such-mode"'),
                    ('graph', 'not json');"#,
            )
        })
        .unwrap();
        let settings = db.settings().unwrap();
        assert_eq!(settings.theme, Theme::Dark);
        assert_eq!(settings.diff_view_mode, DiffViewMode::default());
        assert_eq!(settings.graph, Default::default());
        // Saving keeps the unknown key.
        db.save_settings(&settings).unwrap();
        let kept: String = db
            .with_conn(|c| c.query_row("SELECT value FROM settings WHERE key = 'from_the_future'", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(kept, r#"{"x": 1}"#);
    }

    #[test]
    fn open_tabs_round_trip_and_cascade() {
        let db = db();
        let a = db.insert_repository("a", Path::new("/r/a"), None).unwrap().id;
        let b = db.insert_repository("b", Path::new("/r/b"), None).unwrap().id;
        let tabs = OpenTabs {
            open: vec![b.clone(), a.clone()],
            active: Some(a.clone()),
        };
        db.set_open_tabs(&tabs).unwrap();
        assert_eq!(db.open_tabs().unwrap(), tabs);

        assert!(matches!(
            db.set_open_tabs(&OpenTabs {
                open: vec![a.clone(), a.clone()],
                active: None
            }),
            Err(GitYasError::InvalidInput(_))
        ));
        assert!(matches!(
            db.set_open_tabs(&OpenTabs {
                open: vec![],
                active: Some("ghost".into())
            }),
            Err(GitYasError::InvalidInput(_))
        ));
        assert_eq!(db.open_tabs().unwrap(), tabs);

        db.delete_repository(&a).unwrap();
        assert_eq!(
            db.open_tabs().unwrap(),
            OpenTabs {
                open: vec![b.clone()],
                active: None
            }
        );
    }
}
