//! Git operations and the merge/diff engine. Synchronous and stateless: every function takes a repository
//! path (or an opened repository) and returns values; no globals, no events, no SQLite.
//!
//! Two backends: gitoxide (`gix`, see [`gitoxide`]) reads history, refs, branches and graph queries
//! ([`commits`], [`cache`], [`refscan`], [`refs`] listing, ahead/behind, merge-base) and fetches / clones;
//! libgit2 (`git2`) does everything that writes the working tree or index, and push. History functions take
//! either repository type ([`gitoxide::GixRepo`]).
//!
//! The `git` CLI is never run for repository operations outside `#[cfg(test)]`. Fetch / clone run the programs a
//! transport needs, as git does: the system `ssh` for SSH remotes, `git upload-pack` for local-path remotes, and
//! the configured credential helpers; when `ssh` or `git` is missing they fall back to libgit2. Conversions into
//! `gityas_core` models are free functions here (the orphan rule keeps them out of `gityas-core`).
//!
//! Modules:
//! * Repositories and remotes: [`repository`], [`branches`], [`refs`], [`stash`], [`remote`] (callbacks,
//!   fetch/push/merge)
//! * History and working tree: [`commits`], [`status`], [`worktree`], [`cache`], [`refscan`] (one read of every
//!   ref, reused between refreshes); linked worktrees: [`worktrees`]
//! * Diffs and merges: [`diff`], [`merge`]; one file's history and blame: [`history`]
//! * Shared: [`convert`] (oid conversions, `hunk_content_hash` used by both diffs and hunk staging)

pub mod branches;
pub mod cache;
pub mod commits;
pub mod config;
pub mod convert;
pub mod diff;
pub mod gitoxide;
pub mod history;
pub mod merge;
pub mod refs;
pub mod refscan;
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
