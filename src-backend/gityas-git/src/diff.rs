//! WP6: structured diff, `get_file_diff(DiffTarget, Delta) -> FileDiff`, built from **one** `git2::Patch`
//! (replaces the old `git_rs/files.rs::get_commit_file_diff`).
//!
//! Everything shown comes from the same patch the hunks come from (bug-log #9):
//! * `hunks` and their `DiffLine`s are the patch's hunks/lines (options from
//!   [`crate::convert::apply_hunk_diff_options`], shared with WP5 hunk staging);
//! * `old_text` is the old-side blob (the patch's preimage), and `new_text` is **rebuilt by applying the
//!   patch's hunks to `old_text`**, so it is exactly the postimage libgit2 diffed (after clean filters such as
//!   `core.autocrlf`), never a separate read of the working tree that could disagree. Every context/deleted line
//!   is checked against `old_text` while rebuilding; a mismatch is an error, not a silently wrong view.
//!
//! What is diffed:
//! * `WorkingTree { staged: false }`: index → working tree, untracked files included (one all-added hunk).
//! * `WorkingTree { staged: true }`: `HEAD` tree → index; unborn `HEAD` = empty tree.
//! * `Commit(oid)`: first parent's tree → commit tree (merge commits: **first parent only**, i.e. "what this
//!   merge brought into the branch"); root commits: empty tree.
//! * `Stash(oid)`: stash commit vs its first parent (the stashed tracked changes, staged and unstaged
//!   combined). If the path is not there and the stash has an untracked part (third parent, `stash -u`),
//!   the file is diffed from that tree as an addition.
//!
//! `kind` is never guessed silently (bug-log #7): `Binary` (libgit2's detection: NUL bytes or attributes),
//! `TooLarge { bytes }` (either side over [`DiffConfig::max_bytes`], checked **before** loading contents),
//! `NonUtf8` (`old_text`/`new_text` empty; hunk lines are still present, lossily decoded, so hunk staging keeps
//! working and the UI can offer "show anyway"). Mode-only changes are `Text` with no hunks and equal sides.
//! Submodules show as `Subproject commit <oid>` lines, like git.

use std::path::Path;
use std::sync::Arc;

use git2::{Delta as GitDelta, DiffDelta, DiffOptions, FileMode, Repository};
use gityas_core::diff::{DiffKind, DiffLine, DiffTarget, FileDiff, Hunk, HunkRef, LineOrigin};
use gityas_core::models::{Delta, DeltaChange};
use gityas_core::{GitYasError, Result};

use crate::convert::{apply_hunk_diff_options, hunk_content_hash, oid_to_git, repo_path_bytes};
use crate::git_error;

/// Default size cap per side.
pub const DEFAULT_MAX_DIFF_BYTES: u64 = 5 * 1024 * 1024;

/// Text shown for libgit2's end-of-file markers (`=`, `>`, `<` origins).
pub const NO_NEWLINE_MARKER: &str = "\\ No newline at end of file";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffConfig {
    /// Either side larger than this ⇒ `DiffKind::TooLarge` without loading contents. Raise it for "show anyway".
    pub max_bytes: u64,
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_DIFF_BYTES,
        }
    }
}

/// A raw patch line, kept as bytes until the UTF-8 decision is made.
struct RawLine {
    origin: char,
    old_lineno: Option<u32>,
    new_lineno: Option<u32>,
    content: Vec<u8>,
}

struct RawHunk {
    id: HunkRef,
    header: Vec<u8>,
    lines: Vec<RawLine>,
}

fn empty_diff(kind: DiffKind) -> FileDiff {
    FileDiff {
        old_text: Arc::from(""),
        new_text: Arc::from(""),
        hunks: Vec::new(),
        kind,
        old_mode: None,
        new_mode: None,
    }
}

/// Builds the structured diff of one file. `delta` is what the file lists (WP5) produced: `path` is the new
/// path (old path for deletions), `old_path` is set for renames/copies.
///
/// Errors: `NotFound` if the target has no change for that path (e.g. it was reverted since the list was
/// built; the UI should refresh), `InvalidInput` for conflicted paths (use `merge::get_merge_conflict`) and for
/// working-tree targets in a bare repository.
pub fn get_file_diff(repo: &Repository, target: &DiffTarget, delta: &Delta, config: &DiffConfig) -> Result<FileDiff> {
    if delta.change == DeltaChange::Conflicted {
        return Err(GitYasError::InvalidInput(format!(
            "{} is in conflict; open it in the merge editor",
            delta.path.display()
        )));
    }
    let want_new = repo_path_bytes(&delta.path)?;
    let want_old = delta.old_path.as_deref().map(repo_path_bytes).transpose()?;

    let base_opts = || {
        let mut opts = DiffOptions::new();
        apply_hunk_diff_options(&mut opts)
            .disable_pathspec_match(true)
            .include_typechange(true);
        opts
    };
    let mut paths: Vec<&[u8]> = vec![&want_new[..]];
    paths.extend(want_old.as_deref());
    let mut opts = base_opts();
    for path in &paths {
        opts.pathspec(path);
    }

    let (diff, workdir_new_side) = match target {
        DiffTarget::WorkingTree { staged: false } => {
            if repo.is_bare() {
                return Err(GitYasError::InvalidInput("bare repository has no working tree".into()));
            }
            let workdir_opts = || {
                let mut opts = base_opts();
                opts.include_untracked(true)
                    .recurse_untracked_dirs(true)
                    .show_untracked_content(true);
                opts
            };
            (index_to_workdir(repo, workdir_opts, &paths)?, true)
        }
        DiffTarget::WorkingTree { staged: true } => {
            let head = head_tree(repo)?;
            let index = repo.index().map_err(git_error)?;
            (
                repo.diff_tree_to_index(head.as_ref(), Some(&index), Some(&mut opts))
                    .map_err(git_error)?,
                false,
            )
        }
        DiffTarget::Commit(oid) => {
            let commit = repo.find_commit(oid_to_git(oid)?).map_err(git_error)?;
            let parent = if commit.parent_count() > 0 {
                Some(commit.parent(0).map_err(git_error)?.tree().map_err(git_error)?)
            } else {
                None
            };
            let tree = commit.tree().map_err(git_error)?;
            (
                repo.diff_tree_to_tree(parent.as_ref(), Some(&tree), Some(&mut opts))
                    .map_err(git_error)?,
                false,
            )
        }
        DiffTarget::Stash(oid) => {
            let stash = repo.find_commit(oid_to_git(oid)?).map_err(git_error)?;
            let base = stash.parent(0).map_err(git_error)?.tree().map_err(git_error)?;
            let tree = stash.tree().map_err(git_error)?;
            let mut diff = repo
                .diff_tree_to_tree(Some(&base), Some(&tree), Some(&mut opts))
                .map_err(git_error)?;
            if want_old.is_some() {
                find_renames(&mut diff, delta.change)?;
            }
            if find_delta(&diff, &want_new, want_old.as_deref()).is_none() && stash.parent_count() >= 3 {
                // `git stash -u`: untracked files live in the third parent's tree (a root commit).
                let untracked = stash.parent(2).map_err(git_error)?.tree().map_err(git_error)?;
                diff = repo.diff_tree_to_tree(None, Some(&untracked), Some(&mut opts)).map_err(git_error)?;
            }
            (diff, false)
        }
    };
    let mut diff = diff;
    if want_old.is_some() && !matches!(target, DiffTarget::Stash(_)) {
        find_renames(&mut diff, delta.change)?;
    }
    let idx = find_delta(&diff, &want_new, want_old.as_deref())
        .ok_or_else(|| GitYasError::NotFound(format!("no changes for {} in this diff", delta.path.display())))?;
    let git_delta = diff
        .get_delta(idx)
        .ok_or_else(|| GitYasError::NotFound(format!("no changes for {}", delta.path.display())))?;
    if git_delta.status() == GitDelta::Conflicted {
        return Err(GitYasError::InvalidInput(format!(
            "{} is in conflict; open it in the merge editor",
            delta.path.display()
        )));
    }

    let (old_mode, new_mode) = (side_mode(&git_delta.old_file()), side_mode(&git_delta.new_file()));
    let mut out = file_diff_of(repo, &diff, idx, &git_delta, workdir_new_side, config)?;
    out.old_mode = old_mode;
    out.new_mode = new_mode;
    Ok(out)
}

/// Index → working tree diff limited to `paths` (repository-relative, as bytes); `options` builds the options
/// without any pathspec. libgit2 drops untracked files from a path-limited working-tree diff when a directory
/// above them contains a `.git` entry (e.g. an empty `.git` folder left in a tracked subdirectory), although
/// status still lists them. When none of `paths` is in the limited diff, the whole working tree is diffed instead;
/// callers pick their delta by path, so the extra deltas are never used.
pub(crate) fn index_to_workdir<'r>(repo: &'r Repository, options: impl Fn() -> DiffOptions, paths: &[&[u8]]) -> Result<git2::Diff<'r>> {
    let mut limited = options();
    for path in paths {
        limited.pathspec(path);
    }
    let diff = repo.diff_index_to_workdir(None, Some(&mut limited)).map_err(git_error)?;
    let found = diff.deltas().any(|d| {
        [d.new_file().path_bytes(), d.old_file().path_bytes()]
            .into_iter()
            .flatten()
            .any(|p| paths.contains(&p))
    });
    if found {
        return Ok(diff);
    }
    repo.diff_index_to_workdir(None, Some(&mut options())).map_err(git_error)
}

/// Git file mode of one side, `None` when that side doesn't exist (added / deleted / untracked).
fn side_mode(file: &git2::DiffFile<'_>) -> Option<u32> {
    let mode = u32::from(file.mode());
    (file.exists() && mode != 0).then_some(mode)
}

/// Contents and hunks of delta `idx` of `diff` (modes are filled in by [`get_file_diff`]).
fn file_diff_of(
    repo: &Repository,
    diff: &git2::Diff<'_>,
    idx: usize,
    git_delta: &git2::DiffDelta<'_>,
    workdir_new_side: bool,
    config: &DiffConfig,
) -> Result<FileDiff> {
    // Size cap before anything is loaded.
    let old_size = side_size(repo, git_delta, Side::Old, None)?;
    let new_size = side_size(repo, git_delta, Side::New, workdir_new_side.then(|| repo.workdir()).flatten())?;
    let largest = old_size.max(new_size);
    if largest > config.max_bytes {
        return Ok(empty_diff(DiffKind::TooLarge { bytes: largest }));
    }

    let old_bytes = old_side_bytes(repo, git_delta)?;
    let patch = git2::Patch::from_diff(diff, idx).map_err(git_error)?;
    let Some(patch) = patch else {
        // libgit2 returns no patch for binary or content-unchanged (e.g. mode-only) deltas.
        if git_delta.flags().is_binary() {
            return Ok(empty_diff(DiffKind::Binary));
        }
        return finish(old_bytes.clone(), old_bytes, Vec::new());
    };
    if patch.delta().flags().is_binary() {
        return Ok(empty_diff(DiffKind::Binary));
    }
    let hunks = raw_hunks(&patch)?;
    let new_bytes = apply_hunks(&old_bytes, &hunks)?;
    finish(old_bytes, new_bytes, hunks)
}

fn head_tree(repo: &Repository) -> Result<Option<git2::Tree<'_>>> {
    match repo.head() {
        Ok(head) => Ok(Some(head.peel_to_tree().map_err(git_error)?)),
        Err(e) if matches!(e.code(), git2::ErrorCode::UnbornBranch | git2::ErrorCode::NotFound) => Ok(None),
        Err(e) => Err(git_error(e)),
    }
}

fn find_renames(diff: &mut git2::Diff<'_>, change: DeltaChange) -> Result<()> {
    let mut find = git2::DiffFindOptions::new();
    find.renames(true).for_untracked(true).copies(change == DeltaChange::Copied);
    diff.find_similar(Some(&mut find)).map_err(git_error)
}

/// The delta for `new` (matched byte-exactly on either side's path), preferring an exact `old → new` pair.
fn find_delta(diff: &git2::Diff<'_>, new: &[u8], old: Option<&[u8]>) -> Option<usize> {
    let deltas: Vec<_> = diff.deltas().collect();
    if let Some(old) = old
        && let Some(i) = deltas
            .iter()
            .position(|d| d.new_file().path_bytes() == Some(new) && d.old_file().path_bytes() == Some(old))
    {
        return Some(i);
    }
    deltas
        .iter()
        .position(|d| d.new_file().path_bytes() == Some(new))
        .or_else(|| deltas.iter().position(|d| d.old_file().path_bytes() == Some(new)))
}

#[derive(Clone, Copy)]
enum Side {
    Old,
    New,
}

fn side_size(repo: &Repository, delta: &DiffDelta<'_>, side: Side, workdir: Option<&Path>) -> Result<u64> {
    let file = match side {
        Side::Old => delta.old_file(),
        Side::New => delta.new_file(),
    };
    if !file.exists() || file.mode() == FileMode::Commit {
        return Ok(0);
    }
    if let Some(workdir) = workdir {
        let path = file.path().ok_or_else(|| GitYasError::Git("diff entry without a path".into()))?;
        return match std::fs::symlink_metadata(workdir.join(path)) {
            Ok(meta) => Ok(meta.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e.into()),
        };
    }
    let odb = repo.odb().map_err(git_error)?;
    let (size, _) = odb.read_header(file.id()).map_err(git_error)?;
    Ok(size as u64)
}

/// The preimage the patch was computed from: the old blob, `Subproject commit <oid>\n` for submodules, or empty.
fn old_side_bytes(repo: &Repository, delta: &DiffDelta<'_>) -> Result<Vec<u8>> {
    let file = delta.old_file();
    if !file.exists() || file.id().is_zero() {
        return Ok(Vec::new());
    }
    if file.mode() == FileMode::Commit {
        return Ok(format!("Subproject commit {}\n", file.id()).into_bytes());
    }
    Ok(repo.find_blob(file.id()).map_err(git_error)?.content().to_vec())
}

fn raw_hunks(patch: &git2::Patch<'_>) -> Result<Vec<RawHunk>> {
    let mut hunks = Vec::with_capacity(patch.num_hunks());
    for h in 0..patch.num_hunks() {
        let (hunk, line_count) = patch.hunk(h).map_err(git_error)?;
        let mut lines = Vec::with_capacity(line_count);
        for l in 0..line_count {
            let line = patch.line_in_hunk(h, l).map_err(git_error)?;
            lines.push(RawLine {
                origin: line.origin(),
                old_lineno: line.old_lineno(),
                new_lineno: line.new_lineno(),
                content: line.content().to_vec(),
            });
        }
        // The hash contract shared with WP5 (convert::hunk_content_hash): every line of the git2 hunk, in
        // order, as (origin char, raw content bytes incl. newline); EOF-marker lines ('=', '>', '<') included.
        let content_hash = hunk_content_hash(lines.iter().map(|l| (l.origin, l.content.as_slice())));
        hunks.push(RawHunk {
            id: HunkRef {
                old_start: hunk.old_start(),
                old_lines: hunk.old_lines(),
                new_start: hunk.new_start(),
                new_lines: hunk.new_lines(),
                content_hash,
            },
            header: hunk.header().to_vec(),
            lines,
        });
    }
    Ok(hunks)
}

fn inconsistent(what: &str) -> GitYasError {
    GitYasError::Git(format!(
        "diff is inconsistent with the file contents ({what}); refresh and try again"
    ))
}

/// Rebuilds the postimage by applying `hunks` to `old`, checking every context/deleted line.
fn apply_hunks(old: &[u8], hunks: &[RawHunk]) -> Result<Vec<u8>> {
    let old_lines: Vec<&[u8]> = old.split_inclusive(|b| *b == b'\n').collect();
    let mut out = Vec::with_capacity(old.len());
    let mut pos = 0usize;
    for hunk in hunks {
        // Unified-diff convention: a zero-length range starts *after* line `old_start`.
        let start = if hunk.id.old_lines == 0 {
            hunk.id.old_start
        } else {
            hunk.id.old_start.saturating_sub(1)
        };
        let start = usize::try_from(start).map_err(|_| inconsistent("hunk start"))?;
        let before = old_lines.get(pos..start).ok_or_else(|| inconsistent("hunk outside the old file"))?;
        before.iter().for_each(|l| out.extend_from_slice(l));
        pos = start;
        for line in &hunk.lines {
            match line.origin {
                ' ' | '-' => {
                    if old_lines.get(pos) != Some(&line.content.as_slice()) {
                        return Err(inconsistent("context line differs"));
                    }
                    if line.origin == ' ' {
                        out.extend_from_slice(&line.content);
                    }
                    pos += 1;
                }
                '+' => out.extend_from_slice(&line.content),
                '=' | '>' | '<' => {}
                other => return Err(GitYasError::Git(format!("unexpected diff line origin {other:?}"))),
            }
        }
    }
    old_lines
        .get(pos..)
        .ok_or_else(|| inconsistent("hunk past the end of the old file"))?
        .iter()
        .for_each(|l| out.extend_from_slice(l));
    Ok(out)
}

/// Line text without its terminator (`\n` or `\r\n`).
fn strip_eol(bytes: &[u8]) -> &[u8] {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    bytes.strip_suffix(b"\r").unwrap_or(bytes)
}

fn finish(old: Vec<u8>, new: Vec<u8>, hunks: Vec<RawHunk>) -> Result<FileDiff> {
    let old_text = String::from_utf8(old);
    let new_text = String::from_utf8(new);
    let mut all_utf8 = old_text.is_ok() && new_text.is_ok();
    let mut decode = |bytes: &[u8]| match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => {
            // Flagged via `DiffKind::NonUtf8`, so this lossy text is never shown as if it were exact.
            all_utf8 = false;
            String::from_utf8_lossy(bytes).into_owned()
        }
    };
    let mut out_hunks = Vec::with_capacity(hunks.len());
    for hunk in hunks {
        let header = decode(strip_eol(&hunk.header));
        let mut lines = Vec::with_capacity(hunk.lines.len());
        for line in hunk.lines {
            let (origin, text) = match line.origin {
                ' ' => (LineOrigin::Context, decode(strip_eol(&line.content))),
                '+' => (LineOrigin::Add, decode(strip_eol(&line.content))),
                '-' => (LineOrigin::Del, decode(strip_eol(&line.content))),
                '=' | '>' | '<' => (LineOrigin::NoNewlineAtEof, NO_NEWLINE_MARKER.to_owned()),
                other => return Err(GitYasError::Git(format!("unexpected diff line origin {other:?}"))),
            };
            let (old_lineno, new_lineno) = if origin == LineOrigin::NoNewlineAtEof {
                (None, None)
            } else {
                (line.old_lineno, line.new_lineno)
            };
            lines.push(DiffLine {
                origin,
                old_lineno,
                new_lineno,
                text,
            });
        }
        out_hunks.push(Hunk {
            id: hunk.id,
            header,
            lines,
        });
    }
    match (old_text, new_text) {
        (Ok(old), Ok(new)) if all_utf8 => Ok(FileDiff {
            old_text: Arc::from(old),
            new_text: Arc::from(new),
            hunks: out_hunks,
            kind: DiffKind::Text,
            old_mode: None,
            new_mode: None,
        }),
        _ => Ok(FileDiff {
            old_text: Arc::from(""),
            new_text: Arc::from(""),
            hunks: out_hunks,
            kind: DiffKind::NonUtf8,
            old_mode: None,
            new_mode: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(old_start: u32, old_lines: u32, lines: &[(char, &str)]) -> RawHunk {
        RawHunk {
            id: HunkRef {
                old_start,
                old_lines,
                new_start: 0,
                new_lines: 0,
                content_hash: 0,
            },
            header: Vec::new(),
            lines: lines
                .iter()
                .map(|(o, c)| RawLine {
                    origin: *o,
                    old_lineno: None,
                    new_lineno: None,
                    content: c.as_bytes().to_vec(),
                })
                .collect(),
        }
    }

    #[test]
    fn apply_hunks_rebuilds_postimage() -> Result<()> {
        let old = b"a\nb\nc\n";
        assert_eq!(apply_hunks(old, &[raw(2, 1, &[('-', "b\n"), ('+', "B\n")])])?, b"a\nB\nc\n");
        assert_eq!(apply_hunks(old, &[raw(0, 0, &[('+', "z\n")])])?, b"z\na\nb\nc\n");
        assert_eq!(
            apply_hunks(b"", &[raw(0, 0, &[('+', "x"), ('>', "\n\\ No newline at end of file\n")])])?,
            b"x"
        );
        Ok(())
    }

    #[test]
    fn apply_hunks_rejects_mismatched_context() {
        assert!(apply_hunks(b"a\nb\n", &[raw(1, 1, &[('-', "zzz\n")])]).is_err());
        assert!(apply_hunks(b"a\n", &[raw(9, 1, &[('-', "a\n")])]).is_err());
    }

    #[test]
    fn strip_eol_handles_crlf() {
        assert_eq!(strip_eol(b"x\r\n"), b"x");
        assert_eq!(strip_eol(b"x\n"), b"x");
        assert_eq!(strip_eol(b"x"), b"x");
    }
}
