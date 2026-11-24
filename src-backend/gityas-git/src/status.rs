//! Working-tree status, returned as `gityas_core::models::FileState` (no events).
//!
//! Contract (replaces the old `git_files.rs::update_git_status`, which emitted an event and aborted the whole refresh
//! on the first unmapped delta kind):
//! * every libgit2 delta kind maps to a [`DeltaChange`]; no single entry can fail the refresh;
//! * renames are detected HEAD→index and index→workdir (including rewrites);
//! * untracked files are listed individually (untracked directories are recursed), ignored files are excluded;
//! * a conflicted path is listed **once**, in `unstaged`, with `DeltaChange::Conflicted` (that is where the UI
//!   resolves it), never in `staged`;
//! * submodules are included (a moved or dirty submodule shows as `Modified`);
//! * on Windows, a directory junction (or directory symlink) that holds tracked files is walked like a plain
//!   directory, as Git for Windows does; libgit2 alone reports it as an untracked link with everything under it
//!   deleted (see [`follow_linked_dirs`]);
//! * both lists are sorted by path bytes, then by change kind, so the order is deterministic on every platform
//!   (libgit2 sorts case-insensitively on case-insensitive filesystems).
//!
//! Bare repositories have no working tree: `get_status` returns `InvalidInput` instead of libgit2's raw error.
//!
//! [`get_status`] runs on gitoxide: tracked files are compared with their index entries in parallel, untracked
//! files are found by a directory walk at the same time, and HEAD→index runs on its own thread.
//! [`get_status_paths`] limits the index→worktree part to some paths, for updating a known status after a few
//! files changed (the engine's file watcher). [`get_status_libgit2`] is the previous implementation, kept as the
//! test oracle.

#[cfg(windows)]
use std::path::{Path, PathBuf};

use git2::{Repository, Status, StatusOptions};
use gityas_core::models::{Delta, DeltaChange, FileState};
use gityas_core::{GitYasError, Result};

use crate::git_error;
use crate::gitoxide::{GixRepo, gix_error};

/// Repository-relative path from raw git path bytes (`DiffFile::path_bytes`, `StatusEntry::path_bytes`).
/// On Unix the bytes are kept exactly, so non-UTF-8 paths survive a round trip into git2 again. Elsewhere git
/// stores paths as UTF-8; an invalid sequence there is decoded lossily and logged (it cannot round-trip).
pub fn path_from_bytes(bytes: &[u8]) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::path::PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        match std::str::from_utf8(bytes) {
            Ok(s) => std::path::PathBuf::from(s),
            Err(_) => {
                let lossy = String::from_utf8_lossy(bytes).into_owned();
                tracing::warn!(path = %lossy, "non-UTF-8 path in repository decoded lossily");
                std::path::PathBuf::from(lossy)
            }
        }
    }
}

/// Maps a libgit2 delta kind. `Unmodified` is not a change and returns `None`; it only appears when
/// `INCLUDE_UNMODIFIED` is requested, which this crate never does.
pub fn delta_change(delta: git2::Delta) -> Option<DeltaChange> {
    Some(match delta {
        git2::Delta::Unmodified => return None,
        git2::Delta::Added => DeltaChange::Added,
        git2::Delta::Deleted => DeltaChange::Deleted,
        git2::Delta::Modified => DeltaChange::Modified,
        git2::Delta::Renamed => DeltaChange::Renamed,
        git2::Delta::Copied => DeltaChange::Copied,
        git2::Delta::Ignored => DeltaChange::Ignored,
        git2::Delta::Untracked => DeltaChange::Untracked,
        git2::Delta::Typechange => DeltaChange::Typechange,
        git2::Delta::Unreadable => DeltaChange::Unreadable,
        git2::Delta::Conflicted => DeltaChange::Conflicted,
    })
}

/// Converts a libgit2 diff delta. `path` is the new-side path (old side for deletions); `old_path` is set only
/// for renames and copies. Returns `None` for `Unmodified` (see [`delta_change`]).
pub fn delta_from_git(delta: &git2::DiffDelta<'_>) -> Option<Delta> {
    let change = delta_change(delta.status())?;
    let non_empty = |bytes: &[u8]| (!bytes.is_empty()).then(|| path_from_bytes(bytes));
    let new_path = delta.new_file().path_bytes().and_then(non_empty);
    let old_path = delta.old_file().path_bytes().and_then(non_empty);
    let path = match change {
        DeltaChange::Deleted => old_path.clone().or_else(|| new_path.clone()),
        _ => new_path.clone().or_else(|| old_path.clone()),
    }?;
    let old_path = match change {
        DeltaChange::Renamed | DeltaChange::Copied => old_path.filter(|old| *old != path),
        _ => None,
    };
    Some(Delta { path, old_path, change })
}

/// Fails with a clear message for bare repositories (no working tree, no status, no staging).
pub(crate) fn ensure_workdir(repo: &Repository) -> Result<&std::path::Path> {
    repo.workdir()
        .ok_or_else(|| GitYasError::InvalidInput("this is a bare repository; it has no working tree".into()))
}

fn status_options() -> StatusOptions {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .include_unreadable(true)
        .exclude_submodules(false)
        .renames_head_to_index(true)
        .renames_index_to_workdir(true)
        .renames_from_rewrites(true);
    opts
}

/// Working-tree status of `repo` (see the module docs for the exact contract), through gitoxide.
pub fn get_status<R: GixRepo + ?Sized>(repo: &R) -> Result<FileState> {
    let repo = repo.gix()?;
    let workdir = gix_workdir(&repo)?.to_path_buf();
    let rewrites = gix::diff::Rewrites::default();
    let platform = repo
        .status(gix::progress::Discard)
        .map_err(gix_error)?
        .untracked_files(gix::status::UntrackedFiles::Files)
        .index_worktree_submodules(gix::status::Submodule::AsConfigured { check_dirty: true })
        .index_worktree_rewrites(Some(rewrites))
        .tree_index_track_renames(gix::status::tree_index::TrackRenames::Given(rewrites));
    let mut state = FileState::default();
    let mut staged_changes = Vec::new();
    for item in platform.into_iter(None).map_err(gix_error)? {
        match item.map_err(gix_error)? {
            gix::status::Item::IndexWorktree(item) => {
                if let Some(delta) = worktree_delta(&item) {
                    state.unstaged.push(delta);
                }
            }
            gix::status::Item::TreeIndex(change) => staged_changes.push(change),
        }
    }
    // A conflicted path is listed once, in `unstaged`.
    let conflicted: std::collections::HashSet<std::path::PathBuf> = state
        .unstaged
        .iter()
        .filter(|d| d.change == DeltaChange::Conflicted)
        .map(|d| d.path.clone())
        .collect();
    state.unstaged.dedup_by(|a, b| a.change == DeltaChange::Conflicted && a == b);
    state.staged = staged_changes
        .iter()
        .filter_map(staged_delta)
        .filter(|d| !conflicted.contains(&d.path) && d.old_path.as_ref().is_none_or(|old| !conflicted.contains(old)))
        .collect();
    #[cfg(windows)]
    follow_linked_dirs(&workdir, &mut state.unstaged)?;
    #[cfg(not(windows))]
    let _ = workdir;
    sort_deltas(&mut state.staged);
    sort_deltas(&mut state.unstaged);
    state.unstaged.dedup();
    Ok(state)
}

/// The unstaged (index→worktree) part of [`get_status`] for `paths` only (repository-relative files or
/// directories; a directory covers everything under it), sorted. Renames are not paired: callers that need exact
/// rename pairing must use [`get_status`].
pub fn get_status_paths<R: GixRepo + ?Sized>(repo: &R, paths: &[std::path::PathBuf]) -> Result<Vec<Delta>> {
    let repo = repo.gix()?;
    gix_workdir(&repo)?;
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let patterns: Vec<gix::bstr::BString> = paths
        .iter()
        .map(|p| format!(":(literal){}", p.to_string_lossy().replace('\\', "/")).into())
        .collect();
    let platform = repo
        .status(gix::progress::Discard)
        .map_err(gix_error)?
        .untracked_files(gix::status::UntrackedFiles::Files)
        .index_worktree_submodules(gix::status::Submodule::AsConfigured { check_dirty: true })
        .index_worktree_rewrites(None);
    let mut out = Vec::new();
    for item in platform.into_index_worktree_iter(patterns).map_err(gix_error)? {
        if let Some(delta) = worktree_delta(&item.map_err(gix_error)?) {
            out.push(delta);
        }
    }
    sort_deltas(&mut out);
    out.dedup();
    Ok(out)
}

/// `rel` (repository-relative) or one of its parent directories is a directory link (a Windows junction or
/// directory symlink): its status needs the whole-tree pass of [`get_status`] (see `follow_linked_dirs`). Always
/// `false` elsewhere, where links are plain entries for both libgit2 and gitoxide.
pub fn under_linked_dir(workdir: &std::path::Path, rel: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        rel.ancestors()
            .filter(|dir| !dir.as_os_str().is_empty())
            .any(|dir| is_linked_dir(&workdir.join(dir)))
    }
    #[cfg(not(windows))]
    {
        let _ = (workdir, rel);
        false
    }
}

fn gix_workdir(repo: &gix::Repository) -> Result<&std::path::Path> {
    repo.workdir()
        .ok_or_else(|| GitYasError::InvalidInput("this is a bare repository; it has no working tree".into()))
}

/// One index→worktree item, or `None` when it is no change (stat refresh, tracked directory contents).
fn worktree_delta(item: &gix::status::index_worktree::Item) -> Option<Delta> {
    use gix::status::index_worktree::Item;
    use gix::status::plumbing::index_as_worktree::{Change, EntryStatus};
    match item {
        Item::Modification { rela_path, status, .. } => {
            let change = match status {
                EntryStatus::Conflict { .. } => DeltaChange::Conflicted,
                EntryStatus::Change(Change::Removed) => DeltaChange::Deleted,
                EntryStatus::Change(Change::Type { .. }) => DeltaChange::Typechange,
                EntryStatus::Change(Change::Modification { .. }) => DeltaChange::Modified,
                EntryStatus::Change(Change::SubmoduleModification(change)) => {
                    if change.is_dirty() != Some(true) {
                        return None;
                    }
                    DeltaChange::Modified
                }
                EntryStatus::IntentToAdd => DeltaChange::Added,
                EntryStatus::NeedsUpdate(_) => return None,
            };
            Some(Delta {
                path: path_from_bytes(rela_path),
                old_path: None,
                change,
            })
        }
        Item::DirectoryContents { entry, .. } => (entry.status == gix::dir::entry::Status::Untracked).then(|| Delta {
            path: untracked_path(entry),
            old_path: None,
            change: DeltaChange::Untracked,
        }),
        Item::Rewrite {
            source,
            dirwalk_entry,
            copy,
            ..
        } => Some(Delta {
            path: untracked_path(dirwalk_entry),
            old_path: Some(path_from_bytes(source.rela_path())),
            change: if *copy { DeltaChange::Copied } else { DeltaChange::Renamed },
        }),
    }
}

/// libgit2 names an untracked directory (a nested repository) with a trailing `/`.
fn untracked_path(entry: &gix::dir::Entry) -> std::path::PathBuf {
    let dir = matches!(
        entry.disk_kind,
        Some(gix::dir::entry::Kind::Directory | gix::dir::entry::Kind::Repository)
    );
    if dir {
        let mut bytes = entry.rela_path.to_vec();
        bytes.push(b'/');
        path_from_bytes(&bytes)
    } else {
        path_from_bytes(&entry.rela_path)
    }
}

/// One HEAD→index change.
fn staged_delta(change: &gix::diff::index::Change) -> Option<Delta> {
    use gix::diff::index::ChangeRef;
    let (path, old_path, change) = match change {
        ChangeRef::Addition { location, .. } => (location.as_ref(), None, DeltaChange::Added),
        ChangeRef::Deletion { location, .. } => (location.as_ref(), None, DeltaChange::Deleted),
        ChangeRef::Modification {
            location,
            previous_entry_mode,
            entry_mode,
            ..
        } => {
            let change = if mode_kind(*previous_entry_mode) == mode_kind(*entry_mode) {
                DeltaChange::Modified
            } else {
                DeltaChange::Typechange
            };
            (location.as_ref(), None, change)
        }
        ChangeRef::Rewrite {
            source_location,
            location,
            copy,
            ..
        } => (
            location.as_ref(),
            Some(source_location.as_ref()),
            if *copy { DeltaChange::Copied } else { DeltaChange::Renamed },
        ),
    };
    Some(Delta {
        path: path_from_bytes(path),
        old_path: old_path.map(|p| path_from_bytes(p)),
        change,
    })
}

/// File kind of an index mode: regular file (executable or not), symlink, submodule, or directory.
fn mode_kind(mode: gix::index::entry::Mode) -> u8 {
    use gix::index::entry::Mode;
    if mode.contains(Mode::SYMLINK) {
        1
    } else if mode.contains(Mode::COMMIT) {
        2
    } else if mode.contains(Mode::DIR) {
        3
    } else {
        0
    }
}

/// The previous libgit2 implementation of [`get_status`] (same contract): the oracle of the parity tests.
#[doc(hidden)]
pub fn get_status_libgit2(repo: &Repository) -> Result<FileState> {
    #[cfg_attr(not(windows), allow(unused_variables))]
    let workdir = ensure_workdir(repo)?;
    let mut opts = status_options();
    let statuses = repo.statuses(Some(&mut opts)).map_err(git_error)?;

    let mut state = FileState::default();
    for entry in statuses.iter() {
        if entry.status().contains(Status::CONFLICTED) {
            state.unstaged.push(Delta {
                path: path_from_bytes(entry.path_bytes()),
                old_path: None,
                change: DeltaChange::Conflicted,
            });
            continue;
        }
        if let Some(delta) = entry.head_to_index().as_ref().and_then(delta_from_git) {
            state.staged.push(delta);
        }
        if let Some(delta) = entry.index_to_workdir().as_ref().and_then(delta_from_git) {
            state.unstaged.push(delta);
        }
    }
    #[cfg(windows)]
    follow_linked_dirs(workdir, &mut state.unstaged)?;
    sort_deltas(&mut state.staged);
    sort_deltas(&mut state.unstaged);
    Ok(state)
}

/// libgit2 on Windows lstat()s junctions (and directory symlinks) as symlinks, so a tracked tree reached through one
/// (e.g. `.claude/skills/x -> .agents/skills/x`, as tool installers create) shows as one untracked "file" plus every
/// tracked file under it deleted, and neither can be staged. Git for Windows treats junctions as directories, so for
/// every untracked directory link that holds index entries this re-derives the unstaged state of its contents the
/// way `git status` would: tracked files are re-checked through the link (stat, then filtered content hash), and
/// files the index does not know are listed as untracked unless ignored.
#[cfg(windows)]
fn follow_linked_dirs(workdir: &Path, unstaged: &mut Vec<Delta>) -> Result<()> {
    let links: Vec<PathBuf> = unstaged
        .iter()
        .filter(|d| d.change == DeltaChange::Untracked && is_linked_dir(&workdir.join(&d.path)))
        .map(|d| d.path.clone())
        .collect();
    if links.is_empty() {
        return Ok(());
    }
    // Rare (Windows directory links holding tracked files): handled with libgit2.
    let repo = Repository::open(workdir).map_err(git_error)?;
    let repo = &repo;
    let index = repo.index().map_err(git_error)?;
    for link in links {
        let mut prefix = link.as_os_str().as_encoded_bytes().to_vec();
        prefix.push(b'/');
        let tracked: Vec<git2::IndexEntry> = index
            .iter()
            .filter(|e| e.path.starts_with(&prefix) && index_stage(e) == 0)
            .collect();
        if tracked.is_empty() {
            continue; // a link with nothing tracked under it really is an untracked link
        }
        unstaged.retain(|d| d.path != link && !d.path.as_os_str().as_encoded_bytes().starts_with(&prefix));
        for entry in &tracked {
            let path = path_from_bytes(&entry.path);
            let change = match std::fs::metadata(workdir.join(&path)) {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(DeltaChange::Deleted),
                Err(err) => return Err(err.into()),
                Ok(meta) if meta.is_dir() => Some(DeltaChange::Typechange),
                Ok(meta) => tracked_file_changed(repo, workdir, entry, &path, &meta)?.then_some(DeltaChange::Modified),
            };
            if let Some(change) = change {
                unstaged.push(Delta {
                    path,
                    old_path: None,
                    change,
                });
            }
        }
        let known: std::collections::HashSet<&[u8]> = tracked.iter().map(|e| e.path.as_slice()).collect();
        collect_untracked(repo, workdir, &link, &known, unstaged)?;
    }
    Ok(())
}

/// Is `full` a link (junction or directory symlink) to an existing directory?
#[cfg(windows)]
pub(crate) fn is_linked_dir(full: &Path) -> bool {
    std::fs::symlink_metadata(full).is_ok_and(|m| m.file_type().is_symlink()) && std::fs::metadata(full).is_ok_and(|m| m.is_dir())
}

/// The outermost directory links in the working tree that index entries sit under (see [`follow_linked_dirs`]).
#[cfg(windows)]
pub(crate) fn tracked_linked_dirs(workdir: &Path, index: &git2::Index) -> Vec<PathBuf> {
    let mut checked = std::collections::HashMap::<PathBuf, bool>::new();
    let mut links = std::collections::BTreeSet::new();
    for entry in index.iter().filter(|e| index_stage(e) == 0) {
        let path = path_from_bytes(&entry.path);
        let mut dirs: Vec<&Path> = path.ancestors().skip(1).take_while(|d| !d.as_os_str().is_empty()).collect();
        dirs.reverse();
        for dir in dirs {
            let linked = *checked
                .entry(dir.to_path_buf())
                .or_insert_with(|| is_linked_dir(&workdir.join(dir)));
            if linked {
                links.insert(dir.to_path_buf());
                break;
            }
        }
    }
    links.into_iter().collect()
}

#[cfg(windows)]
pub(crate) fn index_stage(entry: &git2::IndexEntry) -> u16 {
    (entry.flags >> 12) & 0x3
}

/// Has the tracked file at `path` changed against its index entry? Equal size and mtime count as unchanged (git's
/// stat shortcut); otherwise the content is hashed with the repository's filters (autocrlf, attributes). Hashing
/// goes through `blob_path`, which stores the blob if the ODB lacks it, the same object `git add` would write.
#[cfg(windows)]
pub(crate) fn tracked_file_changed(
    repo: &Repository,
    workdir: &Path,
    entry: &git2::IndexEntry,
    path: &Path,
    meta: &std::fs::Metadata,
) -> Result<bool> {
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
    let same_stat = mtime.is_some_and(|m| {
        i64::try_from(m.as_secs()).is_ok_and(|s| s == i64::from(entry.mtime.seconds())) && m.subsec_nanos() == entry.mtime.nanoseconds()
    }) && u64::from(entry.file_size) == meta.len();
    if same_stat {
        return Ok(false);
    }
    let oid = repo.blob_path(&workdir.join(path)).map_err(git_error)?;
    Ok(oid != entry.id)
}

/// Lists files under `dir` (followed even though it is a link) that are neither in `known` nor ignored. Nested
/// links are listed as entries, not followed, matching how libgit2 treats them everywhere else.
#[cfg(windows)]
fn collect_untracked(
    repo: &Repository,
    workdir: &Path,
    dir: &Path,
    known: &std::collections::HashSet<&[u8]>,
    unstaged: &mut Vec<Delta>,
) -> Result<()> {
    for item in std::fs::read_dir(workdir.join(dir))? {
        let item = item?;
        let name = item.file_name();
        if name == ".git" {
            continue;
        }
        let rel = dir.join(&name);
        let rel_git = rel.to_string_lossy().replace('\\', "/");
        let is_dir = item.file_type()?.is_dir();
        let probe = if is_dir { format!("{rel_git}/") } else { rel_git.clone() };
        if repo.is_path_ignored(Path::new(&probe)).map_err(git_error)? {
            continue;
        }
        let rel = PathBuf::from(&rel_git);
        if is_dir {
            collect_untracked(repo, workdir, &rel, known, unstaged)?;
        } else if !known.contains(rel_git.as_bytes()) {
            unstaged.push(Delta {
                path: rel,
                old_path: None,
                change: DeltaChange::Untracked,
            });
        }
    }
    Ok(())
}

/// Sorts by path bytes, then change kind (the `FileState` ordering contract).
pub fn sort_deltas(deltas: &mut [Delta]) {
    deltas.sort_by(|a, b| {
        a.path
            .as_os_str()
            .as_encoded_bytes()
            .cmp(b.path.as_os_str().as_encoded_bytes())
            .then_with(|| change_rank(a.change).cmp(&change_rank(b.change)))
    });
}

fn change_rank(change: DeltaChange) -> u8 {
    match change {
        DeltaChange::Conflicted => 0,
        DeltaChange::Deleted => 1,
        DeltaChange::Renamed => 2,
        DeltaChange::Copied => 3,
        DeltaChange::Typechange => 4,
        DeltaChange::Modified => 5,
        DeltaChange::Added => 6,
        DeltaChange::Untracked => 7,
        DeltaChange::Unreadable => 8,
        DeltaChange::Ignored => 9,
    }
}
