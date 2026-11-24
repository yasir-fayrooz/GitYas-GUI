//! git2 operations and the merge/diff engine. Synchronous and stateless: every function takes a repository
//! path (or an opened `git2::Repository`) and returns values; no globals, no events, no SQLite.
//!
//! Never shells out to the `git` CLI outside `#[cfg(test)]`. Conversions from `git2` types into
//! `gityas_core` models are free functions here (the orphan rule keeps them out of `gityas-core`).
//!
//! Modules:
//! * Repositories and remotes: [`repository`], [`branches`], [`refs`], [`stash`], [`remote`] (callbacks,
//!   fetch/push/merge)
//! * History and working tree: [`commits`], [`status`], [`worktree`], [`cache`]; linked worktrees: [`worktrees`]
//! * Diffs and merges: [`diff`], [`merge`]
//! * Shared: [`convert`] (oid conversions, `hunk_content_hash` used by both diffs and hunk staging)

pub mod branches;
pub mod cache;
pub mod commits;
pub mod config;
pub mod convert;
pub mod diff;
pub mod merge;
pub mod refs;
pub mod remote;
pub mod repository;
pub mod stash;
pub mod status;
pub mod worktree;
pub mod worktrees;

/// Maps a libgit2 error into the shared error type.
pub fn git_error(err: git2::Error) -> gityas_core::GitYasError {
    gityas_core::GitYasError::Git(err.message().to_owned())
}
