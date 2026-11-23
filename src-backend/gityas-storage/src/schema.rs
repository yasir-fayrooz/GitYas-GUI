//! Schemas, versioned through `PRAGMA user_version` by `rusqlite_migration`.
//!
//! User DB: append migrations only (never edit a shipped one). Cache DB: any `user_version` other than
//! [`CACHE_VERSION`] (or 0 = new) makes the file be deleted and recreated, so the cache schema may simply be
//! edited in place as long as `CACHE_VERSION` is bumped by adding a migration.

use rusqlite_migration::{M, Migrations};

const USER_V1: &str = "
CREATE TABLE repositories (
    id         TEXT    PRIMARY KEY NOT NULL,
    name       TEXT    NOT NULL,
    path       TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    provider   TEXT    NULL CHECK (provider IN ('github', 'gitlab', 'bitbucket', 'azure_devops')),
    sort_order INTEGER NOT NULL
) STRICT;

-- One JSON value per `Settings` field (key = serde field name). Unknown keys are kept and ignored.
CREATE TABLE settings (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT;

-- Open tabs in tab-bar order. Deleting a repository closes its tab (FK cascade).
CREATE TABLE open_tabs (
    repo_id  TEXT    PRIMARY KEY NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    position INTEGER NOT NULL
) STRICT;

-- Single row (id = 1): the active repository. Deleting it clears the selection (FK SET NULL).
CREATE TABLE active_repository (
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    repo_id TEXT    NULL REFERENCES repositories(id) ON DELETE SET NULL
) STRICT;
";

const CACHE_V1: &str = "
CREATE TABLE repo_cache (
    repo_id          TEXT    PRIMARY KEY NOT NULL,
    refs_fingerprint BLOB    NOT NULL,
    updated_at       INTEGER NOT NULL
) STRICT;

CREATE TABLE ref_tips (
    repo_id TEXT NOT NULL,
    refname TEXT NOT NULL,
    oid     BLOB NOT NULL,
    PRIMARY KEY (repo_id, refname)
) STRICT, WITHOUT ROWID;

-- Append-only (commits are immutable). `unreachable_since` is maintained by gc: set when gc first finds the
-- commit unreachable from the stored ref tips, cleared when it becomes reachable again.
CREATE TABLE commits (
    repo_id           TEXT    NOT NULL,
    oid               BLOB    NOT NULL,
    author_name       TEXT    NOT NULL,
    author_email      TEXT    NOT NULL,
    author_time       INTEGER NOT NULL,
    committer_time    INTEGER NOT NULL,
    summary           TEXT    NOT NULL,
    unreachable_since INTEGER NULL,
    PRIMARY KEY (repo_id, oid)
) STRICT, WITHOUT ROWID;

CREATE TABLE commit_parents (
    repo_id    TEXT    NOT NULL,
    oid        BLOB    NOT NULL,
    idx        INTEGER NOT NULL,
    parent_oid BLOB    NOT NULL,
    PRIMARY KEY (repo_id, oid, idx)
) STRICT, WITHOUT ROWID;

-- Presence marker for a cached file list (distinguishes an empty list from 'not cached').
CREATE TABLE commit_file_lists (
    repo_id   TEXT    NOT NULL,
    oid       BLOB    NOT NULL,
    cached_at INTEGER NOT NULL,
    PRIMARY KEY (repo_id, oid)
) STRICT, WITHOUT ROWID;

-- Paths are raw OS bytes (non-UTF-8 paths survive on Unix). `status` codes: see cache.rs `change_code`.
CREATE TABLE commit_files (
    repo_id  TEXT    NOT NULL,
    oid      BLOB    NOT NULL,
    idx      INTEGER NOT NULL,
    path     BLOB    NOT NULL,
    old_path BLOB    NULL,
    status   INTEGER NOT NULL,
    PRIMARY KEY (repo_id, oid, idx)
) STRICT, WITHOUT ROWID;
";

/// `Commit::lossy` (author/summary were not valid UTF-8).
const CACHE_V2: &str = "ALTER TABLE commits ADD COLUMN lossy INTEGER NOT NULL DEFAULT 0;";

/// `Commit::description` (one-line body preview). Bumping the version recreates older caches, so every row has it.
const CACHE_V3: &str = "ALTER TABLE commits ADD COLUMN description TEXT NOT NULL DEFAULT '';";

pub(crate) fn user_migrations() -> Migrations<'static> {
    Migrations::new(vec![M::up(USER_V1)])
}

pub(crate) fn cache_migrations() -> Migrations<'static> {
    Migrations::new(vec![M::up(CACHE_V1), M::up(CACHE_V2), M::up(CACHE_V3)])
}

/// `user_version` of a current cache DB (= number of cache migrations).
pub(crate) const CACHE_VERSION: i64 = 3;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_valid() {
        assert!(user_migrations().validate().is_ok());
        assert!(cache_migrations().validate().is_ok());
    }
}
