//! Hunk stage / unstage / discard through libgit2 `Repository::apply` (plan §2.10; replaces the old `git diff` +
//! `git apply` subprocesses, bug-log #8).
//!
//! Every operation first recomputes the diff the hunk came from ([`hunk_diff_options`], the options WP6's
//! `get_file_diff` uses) and requires a hunk with **identical ranges and `hunk_content_hash`**; otherwise it fails
//! with `GitYasError::StaleHunk` and changes nothing.
//! * stage: the index→workdir diff is applied to the index with a hunk callback selecting only that hunk;
//! * unstage: the *reversed* HEAD→index diff is applied to the index;
//! * discard: the *reversed* index→workdir diff is applied to the working tree.
//!
//! For the reversed diffs, the reversed hunk must have the swapped ranges and exactly swapped pre-/post-images of the
//! verified hunk, so a file that changed between the two diff computations is also caught. Every apply is first
//! dry-run (`ApplyOptions::check`) and must select exactly one hunk.
//!
//! Untracked files: libgit2 cannot apply an untracked delta to the index (it looks for a preimage), so staging the
//! (single) hunk of an untracked file adds exactly the verified hunk content as a new index entry, and discarding it
//! deletes the file. A file-level mode change in the same delta is applied together with any of its hunks.

use std::cell::Cell;
use std::path::Path;

use git2::{ApplyLocation, ApplyOptions, Diff, DiffFindOptions, Patch, Repository};
use gityas_core::diff::HunkRef;
use gityas_core::models::{Delta, DeltaChange};
use gityas_core::{GitYasError, Result};

use super::{delta_paths, refuse_conflicts, remove_worktree_file};
use crate::commits::head_commit;
use crate::convert::{hunk_content_hash, repo_path_bytes};
use crate::git_error;
use crate::status::ensure_workdir;
use crate::status::path_from_bytes;

type Ranges = (u32, u32, u32, u32);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    /// index → working tree
    Unstaged,
    /// HEAD → index
    Staged,
}

struct HunkInfo {
    ranges: Ranges,
    hash: u64,
    /// Lines of the old side (context + deletions), raw bytes.
    pre: Vec<u8>,
    /// Lines of the new side (context + additions), raw bytes.
    post: Vec<u8>,
}

struct Located {
    status: git2::Delta,
    new_mode: git2::FileMode,
    new_path: Vec<u8>,
    hunks: Vec<HunkInfo>,
}

fn ranges_of(hunk: &git2::DiffHunk<'_>) -> Ranges {
    (hunk.old_start(), hunk.old_lines(), hunk.new_start(), hunk.new_lines())
}

fn swapped((a, b, c, d): Ranges) -> Ranges {
    (c, d, a, b)
}

fn ranges_of_ref(r: &HunkRef) -> Ranges {
    (r.old_start, r.old_lines, r.new_start, r.new_lines)
}

/// Diff options for working-tree hunk diffs. **Must match `diff.rs::get_file_diff` /
/// `convert::apply_hunk_diff_options` (WP6)**, otherwise the same edit yields different hunks and every hunk
/// operation fails with `StaleHunk`: 3 context lines, no inter-hunk merging, indent heuristic on (as the `git diff`
/// CLI), typechange detection, literal (non-glob) paths, untracked files with content. `paths` limits the diff (the
/// delta's path and, for renames/copies, its old path). The untracked options only affect index→workdir diffs.
pub fn hunk_diff_options(paths: &[&Path], reverse: bool) -> git2::DiffOptions {
    let mut opts = git2::DiffOptions::new();
    crate::convert::apply_hunk_diff_options(&mut opts)
        .include_typechange(true)
        .disable_pathspec_match(true)
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true)
        .reverse(reverse);
    for path in paths {
        opts.pathspec(*path);
    }
    opts
}

fn side_diff<'r>(repo: &'r Repository, side: Side, file: &Delta, reverse: bool) -> Result<Diff<'r>> {
    let paths = delta_paths(file)?;
    let mut diff = match side {
        Side::Unstaged => {
            let bytes = paths.iter().map(|p| repo_path_bytes(p)).collect::<Result<Vec<_>>>()?;
            let bytes: Vec<&[u8]> = bytes.iter().map(|b| &b[..]).collect();
            crate::diff::index_to_workdir(repo, || hunk_diff_options(&[], reverse), &bytes)?
        }
        Side::Staged => {
            let tree = match head_commit(repo)? {
                Some(head) => Some(head.tree().map_err(git_error)?),
                None => None,
            };
            repo.diff_tree_to_index(tree.as_ref(), None, Some(&mut hunk_diff_options(&paths, reverse)))
                .map_err(git_error)?
        }
    };
    if file.old_path.is_some() {
        let mut find = DiffFindOptions::new();
        // Same as WP6's `diff.rs::find_renames`.
        find.renames(true).for_untracked(true).copies(file.change == DeltaChange::Copied);
        diff.find_similar(Some(&mut find)).map_err(git_error)?;
    }
    Ok(diff)
}

fn delta_touches(delta: &git2::DiffDelta<'_>, path: &Path) -> bool {
    [delta.new_file().path_bytes(), delta.old_file().path_bytes()]
        .into_iter()
        .flatten()
        .any(|bytes| path_from_bytes(bytes) == path)
}

/// Finds the delta for `path` in `diff` and its hunks. `None` if the path has no (textual) change any more.
fn locate(diff: &Diff<'_>, path: &Path) -> Result<Option<Located>> {
    let Some(idx) = diff.deltas().position(|d| delta_touches(&d, path)) else {
        return Ok(None);
    };
    let Some(patch) = Patch::from_diff(diff, idx).map_err(git_error)? else {
        return Ok(None);
    };
    let delta = patch.delta();
    let status = delta.status();
    let new_mode = delta.new_file().mode();
    let new_path = delta.new_file().path_bytes().unwrap_or_default().to_vec();
    let mut hunks = Vec::with_capacity(patch.num_hunks());
    for h in 0..patch.num_hunks() {
        let (hunk, line_count) = patch.hunk(h).map_err(git_error)?;
        let ranges = ranges_of(&hunk);
        let mut lines = Vec::with_capacity(line_count);
        for l in 0..line_count {
            let line = patch.line_in_hunk(h, l).map_err(git_error)?;
            lines.push((line.origin(), line.content().to_vec()));
        }
        let hash = hunk_content_hash(lines.iter().map(|(o, c)| (*o, c.as_slice())));
        let (pre, post) = images(&lines);
        hunks.push(HunkInfo { ranges, hash, pre, post });
    }
    Ok(Some(Located {
        status,
        new_mode,
        new_path,
        hunks,
    }))
}

/// Pre-/post-image bytes of a hunk. In generated diffs a last line without newline has no `\n` and the end-of-file
/// marker lines (`=`, `<`, `>`) carry no content. In *parsed* patches (`Diff::from_buffer`) that line keeps a `\n`
/// and the marker means "drop it", so a marker strips a trailing newline from the preceding line's side(s).
fn images(lines: &[(char, Vec<u8>)]) -> (Vec<u8>, Vec<u8>) {
    let (mut pre, mut post) = (Vec::new(), Vec::new());
    let mut last = ' ';
    let strip = |buf: &mut Vec<u8>| {
        if buf.last() == Some(&b'\n') {
            buf.pop();
        }
    };
    for (origin, content) in lines {
        match origin {
            ' ' => {
                pre.extend_from_slice(content);
                post.extend_from_slice(content);
            }
            '-' => pre.extend_from_slice(content),
            '+' => post.extend_from_slice(content),
            '=' | '<' | '>' => {
                match last {
                    '-' => strip(&mut pre),
                    '+' => strip(&mut post),
                    _ => {
                        strip(&mut pre);
                        strip(&mut post);
                    }
                }
                continue;
            }
            _ => continue,
        }
        last = *origin;
    }
    (pre, post)
}

/// Recomputes the forward diff and returns it with the delta and the index of the hunk identical to `hunk`.
fn verify<'r>(repo: &'r Repository, side: Side, file: &Delta, hunk: &HunkRef) -> Result<(Diff<'r>, Located, usize)> {
    let diff = side_diff(repo, side, file, false)?;
    let located = locate(&diff, &file.path)?.ok_or(GitYasError::StaleHunk)?;
    let pos = located
        .hunks
        .iter()
        .position(|h| h.ranges == ranges_of_ref(hunk) && h.hash == hunk.content_hash)
        .ok_or(GitYasError::StaleHunk)?;
    Ok((diff, located, pos))
}

/// Applies the single hunk with `ranges` of the delta touching `path` in `diff` to `location`: dry run first,
/// then for real, each time requiring exactly one selected hunk (deletions apply without hunk callbacks).
fn apply_one_hunk(
    repo: &Repository,
    diff: &Diff<'_>,
    location: ApplyLocation,
    path: &Path,
    ranges: Ranges,
    status: git2::Delta,
) -> Result<()> {
    for check in [true, false] {
        let selected = Cell::new(0usize);
        let mut opts = ApplyOptions::new();
        opts.check(check);
        opts.delta_callback(|delta| delta.is_some_and(|d| delta_touches(&d, path)));
        opts.hunk_callback(|hunk| {
            let pick = hunk.is_some_and(|h| ranges_of(&h) == ranges);
            if pick {
                selected.set(selected.get() + 1);
            }
            pick
        });
        repo.apply(diff, location, Some(&mut opts)).map_err(|err| match err.code() {
            git2::ErrorCode::ApplyFail => GitYasError::StaleHunk,
            _ => git_error(err),
        })?;
        drop(opts);
        let expected = usize::from(status != git2::Delta::Deleted);
        if selected.get() != expected {
            return Err(GitYasError::StaleHunk);
        }
    }
    Ok(())
}

/// Reversed diff for `side`, checked to contain the exact inverse of `verified`; returns its status and ranges.
fn reversed_hunk<'r>(
    repo: &'r Repository,
    side: Side,
    file: &Delta,
    forward_status: git2::Delta,
    verified: &HunkInfo,
) -> Result<(Diff<'r>, git2::Delta, Ranges)> {
    let diff = if matches!(forward_status, git2::Delta::Renamed | git2::Delta::Copied) {
        reversed_content_diff(repo, side, file)?
    } else {
        side_diff(repo, side, file, true)?
    };
    let located = locate(&diff, &file.path)?.ok_or(GitYasError::StaleHunk)?;
    let want = swapped(verified.ranges);
    let ok = located
        .hunks
        .iter()
        .any(|h| h.ranges == want && h.pre == verified.post && h.post == verified.pre);
    if !ok {
        return Err(GitYasError::StaleHunk);
    }
    Ok((diff, located.status, want))
}

/// Reversed diff of a renamed/copied file's *content only*, as a modification of `file.path`. A reversed rename
/// delta would also undo the rename when applied, which unstaging/discarding one hunk must not do. Built from the
/// two blobs (`Patch::from_buffers`, same context settings) and parsed back into a `Diff` so `Repository::apply`
/// can use it; the patch's mode is set to the file's current mode so applying keeps it.
fn reversed_content_diff(repo: &Repository, side: Side, file: &Delta) -> Result<Diff<'static>> {
    let old_path = file.old_path.as_deref().ok_or(GitYasError::StaleHunk)?;
    let mut index = repo.index().map_err(git_error)?;
    index.read(false).map_err(git_error)?;
    let blob_content = |id: git2::Oid| -> Result<Vec<u8>> { Ok(repo.find_blob(id).map_err(git_error)?.content().to_vec()) };
    let (current, mode, original) = match side {
        Side::Staged => {
            let entry = index.get_path(&file.path, 0).ok_or(GitYasError::StaleHunk)?;
            let head = head_commit(repo)?.ok_or(GitYasError::StaleHunk)?;
            let tree = head.tree().map_err(git_error)?;
            let original = tree.get_path(old_path).map_err(|_| GitYasError::StaleHunk)?;
            (blob_content(entry.id)?, entry.mode, blob_content(original.id())?)
        }
        Side::Unstaged => {
            let full = ensure_workdir(repo)?.join(&file.path);
            let meta = std::fs::symlink_metadata(&full)?;
            if !meta.is_file() {
                return Err(GitYasError::InvalidInput(format!("{} is not a regular file", file.path.display())));
            }
            // Through the repository's filters, like the forward diff.
            let current = blob_content(repo.blob_path(&full).map_err(git_error)?)?;
            let original = index.get_path(old_path, 0).ok_or(GitYasError::StaleHunk)?;
            (current, u32::from(worktree_mode(&meta)), blob_content(original.id)?)
        }
    };
    let mut opts = git2::DiffOptions::new();
    opts.context_lines(3).interhunk_lines(0).indent_heuristic(true);
    let mut patch = Patch::from_buffers(&current, Some(&file.path), &original, Some(&file.path), Some(&mut opts)).map_err(git_error)?;
    let text = with_patch_mode(&patch.to_buf().map_err(git_error)?, mode);
    Diff::from_buffer(&text).map_err(git_error)
}

#[cfg(unix)]
fn worktree_mode(meta: &std::fs::Metadata) -> git2::FileMode {
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o111 != 0 {
        git2::FileMode::BlobExecutable
    } else {
        git2::FileMode::Blob
    }
}

#[cfg(not(unix))]
fn worktree_mode(_meta: &std::fs::Metadata) -> git2::FileMode {
    git2::FileMode::Blob
}

/// Replaces the mode on the patch header's `index <a>..<b> <mode>` line.
fn with_patch_mode(patch: &[u8], mode: u32) -> Vec<u8> {
    let header_end = find(patch, b"\n--- ").unwrap_or(patch.len());
    let Some(start) = find(patch.get(..header_end).unwrap_or_default(), b"\nindex ").map(|p| p + 1) else {
        return patch.to_vec();
    };
    let line_end = find(patch.get(start..).unwrap_or_default(), b"\n").map_or(patch.len(), |e| start + e);
    let line = patch.get(start..line_end).unwrap_or_default();
    let Some(space) = line.iter().rposition(|&b| b == b' ') else {
        return patch.to_vec();
    };
    let mut out = Vec::with_capacity(patch.len() + 8);
    out.extend_from_slice(patch.get(..start + space + 1).unwrap_or_default());
    out.extend_from_slice(format!("{mode:o}").as_bytes());
    out.extend_from_slice(patch.get(line_end..).unwrap_or_default());
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn check_not_conflicted(repo: &Repository, file: &Delta) -> Result<()> {
    let mut index = repo.index().map_err(git_error)?;
    index.read(false).map_err(git_error)?;
    let paths = delta_paths(file)?;
    refuse_conflicts(&index, file, &paths)
}

/// Stages one hunk of an unstaged file. `hunk` must come from the `WorkingTree { staged: false }` diff.
pub fn stage_hunk(repo: &Repository, file: &Delta, hunk: &HunkRef) -> Result<()> {
    ensure_workdir(repo)?;
    check_not_conflicted(repo, file)?;
    let (diff, located, pos) = verify(repo, Side::Unstaged, file, hunk)?;
    let info = located.hunks.get(pos).ok_or(GitYasError::StaleHunk)?;
    if located.status == git2::Delta::Untracked {
        return stage_untracked(repo, &located, info);
    }
    apply_one_hunk(repo, &diff, ApplyLocation::Index, &file.path, info.ranges, located.status)
}

/// Untracked file: add exactly the verified content (the file's only hunk) as a new index entry.
fn stage_untracked(repo: &Repository, located: &Located, info: &HunkInfo) -> Result<()> {
    if located.hunks.len() != 1 || located.new_path.is_empty() {
        return Err(GitYasError::StaleHunk);
    }
    let entry = git2::IndexEntry {
        ctime: git2::IndexTime::new(0, 0),
        mtime: git2::IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode: u32::from(located.new_mode),
        uid: 0,
        gid: 0,
        file_size: 0,
        id: git2::Oid::zero(),
        flags: 0,
        flags_extended: 0,
        path: located.new_path.clone(),
    };
    let mut index = repo.index().map_err(git_error)?;
    index.read(false).map_err(git_error)?;
    index.add_frombuffer(&entry, &info.post).map_err(git_error)?;
    index.write().map_err(git_error)
}

/// Unstages one hunk of a staged file. `hunk` must come from the `WorkingTree { staged: true }` diff.
pub fn unstage_hunk(repo: &Repository, file: &Delta, hunk: &HunkRef) -> Result<()> {
    ensure_workdir(repo)?;
    check_not_conflicted(repo, file)?;
    let (_, located, pos) = verify(repo, Side::Staged, file, hunk)?;
    let info = located.hunks.get(pos).ok_or(GitYasError::StaleHunk)?;
    let (diff, status, ranges) = reversed_hunk(repo, Side::Staged, file, located.status, info)?;
    apply_one_hunk(repo, &diff, ApplyLocation::Index, &file.path, ranges, status)
}

/// Discards one hunk of an unstaged file from the working tree. `hunk` must come from the
/// `WorkingTree { staged: false }` diff. For an untracked file (a single hunk) the file is deleted.
pub fn discard_hunk(repo: &Repository, file: &Delta, hunk: &HunkRef) -> Result<()> {
    let workdir = ensure_workdir(repo)?.to_path_buf();
    check_not_conflicted(repo, file)?;
    let (_, located, pos) = verify(repo, Side::Unstaged, file, hunk)?;
    let info = located.hunks.get(pos).ok_or(GitYasError::StaleHunk)?;
    if located.status == git2::Delta::Untracked {
        if located.hunks.len() != 1 {
            return Err(GitYasError::StaleHunk);
        }
        return remove_worktree_file(&workdir.join(&file.path));
    }
    let (diff, status, ranges) = reversed_hunk(repo, Side::Unstaged, file, located.status, info)?;
    apply_one_hunk(repo, &diff, ApplyLocation::WorkDir, &file.path, ranges, status)
}
