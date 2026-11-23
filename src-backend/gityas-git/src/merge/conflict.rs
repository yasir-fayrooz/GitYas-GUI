//! Reading and resolving index conflicts (replaces the conflict parts of the old `git_rs/files.rs` and
//! `application/commands/files.rs::resolve_conflict*`).

use std::path::{Component, Path, PathBuf};

use git2::{IndexEntry, Repository};
use gityas_core::diff::DiffKind;
use gityas_core::models::{ConflictChunk, ConflictType, DeletedMergeAction, MergeConflict};
use gityas_core::{GitYasError, Result};

use super::editor::{EditorMerge, editor_chunks};
use super::engine::{TextMerge, merge_texts};
use crate::git_error;

/// Per-side cap for loading conflict contents into the merge editor.
pub const DEFAULT_MAX_CONFLICT_BYTES: u64 = 5 * 1024 * 1024;

const MODE_GITLINK: u32 = 0o160_000;
const MODE_SYMLINK: u32 = 0o120_000;
const MODE_EXEC: u32 = 0o100_755;
const INDEX_STAGE_MASK: u16 = 0x3000;

pub use gityas_core::models::{ConflictSide, ConflictSides};

/// Rejects absolute paths and `..` so a caller can never write outside the working tree.
fn validate_repo_path(path: &Path) -> Result<()> {
    let ok = !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)));
    if ok {
        Ok(())
    } else {
        Err(GitYasError::InvalidInput(format!(
            "not a repository-relative path: {}",
            path.display()
        )))
    }
}

fn fresh_index(repo: &Repository) -> Result<git2::Index> {
    let mut index = repo.index().map_err(git_error)?;
    index.read(false).map_err(git_error)?;
    Ok(index)
}

fn not_conflicted(path: &Path) -> GitYasError {
    GitYasError::NotFound(format!("{} is not in conflict", path.display()))
}

fn read_conflict(index: &git2::Index, path: &Path) -> Result<git2::IndexConflict> {
    match index.conflict_get(path) {
        Ok(c) => Ok(c),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Err(not_conflicted(path)),
        Err(e) => Err(git_error(e)),
    }
}

fn side_info(repo: &Repository, entry: &IndexEntry) -> Result<ConflictSide> {
    if entry.mode == MODE_GITLINK {
        return Ok(ConflictSide {
            mode: entry.mode,
            bytes: 0,
            kind: DiffKind::Binary,
        });
    }
    let blob = repo.find_blob(entry.id).map_err(git_error)?;
    let bytes = blob.size() as u64;
    let kind = if entry.mode == MODE_SYMLINK || blob.is_binary() {
        DiffKind::Binary
    } else if bytes > DEFAULT_MAX_CONFLICT_BYTES {
        DiffKind::TooLarge { bytes }
    } else if std::str::from_utf8(blob.content()).is_err() {
        DiffKind::NonUtf8
    } else {
        DiffKind::Text
    };
    Ok(ConflictSide {
        mode: entry.mode,
        bytes,
        kind,
    })
}

/// Describes the stages of a conflicted path. `NotFound` if `path` is not conflicted.
pub fn conflict_sides(repo: &Repository, path: &Path) -> Result<ConflictSides> {
    validate_repo_path(path)?;
    let conflict = read_conflict(&fresh_index(repo)?, path)?;
    let info = |e: &Option<IndexEntry>| e.as_ref().map(|e| side_info(repo, e)).transpose();
    Ok(ConflictSides {
        ancestor: info(&conflict.ancestor)?,
        ours: info(&conflict.our)?,
        theirs: info(&conflict.their)?,
    })
}

fn load_text(repo: &Repository, entry: &IndexEntry) -> Result<String> {
    let blob = repo.find_blob(entry.id).map_err(git_error)?;
    // Only called for sides `side_info` classified as `Text`, which checked UTF-8; re-check rather than trust it.
    String::from_utf8(blob.content().to_vec()).map_err(|_| GitYasError::InvalidInput("conflict side is not valid UTF-8".into()))
}

fn to_chunks(merge: &TextMerge) -> Vec<ConflictChunk> {
    merge
        .conflicts
        .iter()
        .map(|c| ConflictChunk {
            conflict_type: ConflictType::Both,
            ours_start: c.ours.start,
            ours_end: c.ours.end,
            theirs_start: c.theirs.start,
            theirs_end: c.theirs.end,
            ancestor_start: c.base.start,
            ancestor_end: c.base.end,
            merged_start: c.merged.start,
            merged_end: c.merged.end,
        })
        .collect()
}

/// `split_inclusive('\n')` line count. The editor layout counts `split('\n')` pieces, which only adds the empty
/// piece after a final `\n` (or the one piece of an empty text).
fn inclusive_lines(text: &str) -> usize {
    text.split_inclusive('\n').count()
}

/// Editor layout → chunks in the `split_inclusive` line model (ranges clamped off the final empty piece).
fn editor_to_chunks(layout: &EditorMerge, ancestor: &str, ours: &str, theirs: &str) -> Vec<ConflictChunk> {
    let (nb, no, nt, nm) = (
        inclusive_lines(ancestor),
        inclusive_lines(ours),
        inclusive_lines(theirs),
        inclusive_lines(&layout.merged),
    );
    layout
        .chunks
        .iter()
        .map(|c| ConflictChunk {
            conflict_type: ConflictType::Both,
            ours_start: c.ours.start.min(no),
            ours_end: c.ours.end.min(no),
            theirs_start: c.theirs.start.min(nt),
            theirs_end: c.theirs.end.min(nt),
            ancestor_start: c.base.start.min(nb),
            ancestor_end: c.base.end.min(nb),
            merged_start: c.merged.start.min(nm),
            merged_end: c.merged.end.min(nm),
        })
        .collect()
}

/// Pure three-way merge of already-loaded texts into a [`MergeConflict`] (`ancestor` = `""` when the file was
/// added on both sides). `sides` is left empty: there is no index to describe; [`get_merge_conflict`] fills it.
///
/// Chunks use the old app's layout ([`super::editor`]). git's layout ([`merge_texts`]) is used
/// instead when that finds no conflict (so a change git would not merge silently is never auto-resolved) or gives
/// up (very large rewrites).
pub fn conflict_from_texts(ancestor: &str, ours: &str, theirs: &str) -> Result<MergeConflict> {
    let (merged, chunks) = match editor_chunks(ancestor, ours, theirs).filter(|l| !l.chunks.is_empty()) {
        Some(layout) => {
            let chunks = editor_to_chunks(&layout, ancestor, ours, theirs);
            (layout.merged, chunks)
        }
        None => {
            let merge = merge_texts(ancestor, ours, theirs)?;
            let chunks = to_chunks(&merge);
            (merge.merged, chunks)
        }
    };
    Ok(MergeConflict {
        ancestor: ancestor.to_owned(),
        ours: ours.to_owned(),
        theirs: theirs.to_owned(),
        auto_resolved: chunks.is_empty(),
        chunks,
        merged,
        sides: ConflictSides::default(),
        ours_deleted: false,
        theirs_deleted: false,
    })
}

/// Loads a conflicted path for the merge editor.
///
/// * `sides` always describes each index stage (mode, size, kind).
/// * `ancestor` is the real base (`""` for add/add conflicts, which have no base stage).
/// * A deleted side sets `ours_deleted` / `theirs_deleted`, its text is `""` and the other side's text is
///   filled in; `chunks` and `merged` are empty (the UI shows the delete/modify dialog). Both deleted ⇒
///   `auto_resolved`.
/// * If any present side is not text (binary, symlink, submodule, non-UTF-8, over the cap) the conflict is still
///   returned: text fields of the non-text sides are `""`, `chunks`/`merged` are empty, and `sides.is_text()` is
///   false, so the UI offers [`resolve_conflict_with_action`] instead of the text editor.
/// * `NotFound` if `path` is not conflicted.
pub fn get_merge_conflict(repo: &Repository, path: &Path) -> Result<MergeConflict> {
    validate_repo_path(path)?;
    let conflict = read_conflict(&fresh_index(repo)?, path)?;
    let info = |e: &Option<IndexEntry>| e.as_ref().map(|e| side_info(repo, e)).transpose();
    let sides = ConflictSides {
        ancestor: info(&conflict.ancestor)?,
        ours: info(&conflict.our)?,
        theirs: info(&conflict.their)?,
    };
    let text = |e: &Option<IndexEntry>, side: &Option<ConflictSide>| -> Result<Option<String>> {
        match (e, side) {
            (Some(e), Some(s)) if s.kind == DiffKind::Text => load_text(repo, e).map(Some),
            _ => Ok(None),
        }
    };
    let ancestor = text(&conflict.ancestor, &sides.ancestor)?.unwrap_or_default();
    let ours = text(&conflict.our, &sides.ours)?;
    let theirs = text(&conflict.their, &sides.theirs)?;
    let (ours_deleted, theirs_deleted) = (sides.ours.is_none(), sides.theirs.is_none());
    if let (true, Some(ours), Some(theirs)) = (sides.is_text(), &ours, &theirs) {
        return Ok(MergeConflict {
            sides,
            ..conflict_from_texts(&ancestor, ours, theirs)?
        });
    }
    Ok(MergeConflict {
        ancestor,
        ours: ours.unwrap_or_default(),
        theirs: theirs.unwrap_or_default(),
        merged: String::new(),
        chunks: Vec::new(),
        sides,
        ours_deleted,
        theirs_deleted,
        auto_resolved: ours_deleted && theirs_deleted,
    })
}

fn workdir_path(repo: &Repository, path: &Path) -> Result<PathBuf> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| GitYasError::InvalidInput("repository has no working directory (bare)".into()))?;
    Ok(workdir.join(path))
}

/// Removes whatever is at `full` if it is a file or symlink (never follows the link).
fn remove_workdir_file(full: &Path) -> Result<()> {
    match std::fs::symlink_metadata(full) {
        Ok(meta) if meta.is_dir() => Err(GitYasError::InvalidInput(format!(
            "{} is a directory in the working tree; resolve it manually",
            full.display()
        ))),
        Ok(_) => Ok(std::fs::remove_file(full)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Writes `content` as the resolution of `path`, stages it (which removes the conflict stages) and writes the
/// index. Does **not** end the merge: `MERGE_HEAD` stays so the next commit records both parents (the old code
/// called `cleanup_state()` here, which silently turned the merge into a normal commit).
///
/// If the file did not exist in the working tree, it gets the executable bit when ours (or else theirs) had it.
pub fn resolve_conflict(repo: &Repository, path: &Path, content: &str) -> Result<()> {
    validate_repo_path(path)?;
    let mut index = fresh_index(repo)?;
    let conflict = read_conflict(&index, path)?;
    let full = workdir_path(repo, path)?;
    let existed_as_file = match std::fs::symlink_metadata(&full) {
        Ok(meta) if meta.file_type().is_symlink() => {
            // Never write through a symlink (it could point outside the repository).
            std::fs::remove_file(&full)?;
            false
        }
        Ok(meta) if meta.is_dir() => {
            return Err(GitYasError::InvalidInput(format!(
                "{} is a directory in the working tree",
                path.display()
            )));
        }
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&full, content.as_bytes())?;
    if !existed_as_file {
        let mode = conflict.our.as_ref().or(conflict.their.as_ref()).map(|e| e.mode);
        set_executable(&full, mode == Some(MODE_EXEC))?;
    }
    index.add_path(path).map_err(git_error)?;
    index.write().map_err(git_error)
}

#[cfg(unix)]
fn set_executable(full: &Path, exec: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if exec {
        let mut perms = std::fs::metadata(full)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(full, perms)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_full: &Path, _exec: bool) -> Result<()> {
    Ok(())
}

/// `git add` for conflicted paths ("Mark resolved"): stages each file as it is in the working tree (a missing file
/// stages the deletion), which removes its conflict stages. Content is not checked, so leftover conflict markers are
/// staged too, as with git. All paths are validated and must be in conflict before anything is staged; the index is
/// written once. Like [`resolve_conflict`], it leaves the merge in progress.
pub fn mark_resolved(repo: &Repository, paths: &[PathBuf]) -> Result<()> {
    let mut index = fresh_index(repo)?;
    for path in paths {
        validate_repo_path(path)?;
        read_conflict(&index, path)?;
    }
    for path in paths {
        let full = workdir_path(repo, path)?;
        match std::fs::symlink_metadata(&full) {
            Ok(meta) if meta.is_dir() => {
                return Err(GitYasError::InvalidInput(format!(
                    "{} is a directory in the working tree; resolve it manually",
                    path.display()
                )));
            }
            Ok(_) => index.add_path(path).map_err(git_error)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                index.conflict_remove(path).map_err(git_error)?;
                index.remove_path(path).map_err(git_error)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    index.write().map_err(git_error)
}

/// Resolves a conflict by taking one stage as-is (any content: binary, symlink, submodule), or deleting the
/// file. Taking a version that does not exist (e.g. `TakeOurs` when ours deleted the file, `TakeBase` for an
/// add/add conflict) deletes the file. The chosen blob is staged with its original mode and checked out
/// through libgit2 (so filters, the executable bit and symlinks are honoured, and binary data is never
/// re-encoded). Like [`resolve_conflict`], it leaves the merge in progress.
pub fn resolve_conflict_with_action(repo: &Repository, path: &Path, action: DeletedMergeAction) -> Result<()> {
    validate_repo_path(path)?;
    let mut index = fresh_index(repo)?;
    let conflict = read_conflict(&index, path)?;
    let chosen = match action {
        DeletedMergeAction::TakeOurs => conflict.our,
        DeletedMergeAction::TakeTheirs => conflict.their,
        DeletedMergeAction::TakeBase => conflict.ancestor,
        DeletedMergeAction::DeleteFile => None,
    };
    let full = workdir_path(repo, path)?;
    index.conflict_remove(path).map_err(git_error)?;
    match chosen {
        None => {
            remove_workdir_file(&full)?;
            index.write().map_err(git_error)?;
        }
        Some(mut entry) => {
            entry.flags &= !INDEX_STAGE_MASK;
            index.add(&entry).map_err(git_error)?;
            index.write().map_err(git_error)?;
            if entry.mode != MODE_GITLINK {
                remove_workdir_file(&full)?;
                let mut checkout = git2::build::CheckoutBuilder::new();
                checkout.force().disable_pathspec_match(true).path(path);
                repo.checkout_index(Some(&mut index), Some(&mut checkout)).map_err(git_error)?;
            }
        }
    }
    Ok(())
}
