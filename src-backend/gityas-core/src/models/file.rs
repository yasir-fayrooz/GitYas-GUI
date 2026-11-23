use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Change kind of a file. Every libgit2 delta maps to a variant, so one unusual entry can never abort a
/// status refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaChange {
    Added,
    Untracked,
    Copied,
    Deleted,
    Renamed,
    Modified,
    Conflicted,
    Typechange,
    Ignored,
    Unreadable,
}

/// One changed file. Paths are repository-relative.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Delta {
    /// New-side path (old-side for deletions).
    pub path: PathBuf,
    /// Set for renames/copies.
    pub old_path: Option<PathBuf>,
    pub change: DeltaChange,
}

/// Working-tree status of the active repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    pub unstaged: Vec<Delta>,
    pub staged: Vec<Delta>,
}

impl DeltaChange {
    pub const ALL: [DeltaChange; 10] = [
        DeltaChange::Added,
        DeltaChange::Untracked,
        DeltaChange::Copied,
        DeltaChange::Deleted,
        DeltaChange::Renamed,
        DeltaChange::Modified,
        DeltaChange::Conflicted,
        DeltaChange::Typechange,
        DeltaChange::Ignored,
        DeltaChange::Unreadable,
    ];

    /// One-letter badge for file lists, close to `git status --short` (`?` untracked, `U` conflicted,
    /// `!` ignored; `X` unreadable has no git equivalent).
    pub fn code(self) -> char {
        match self {
            DeltaChange::Added => 'A',
            DeltaChange::Untracked => '?',
            DeltaChange::Copied => 'C',
            DeltaChange::Deleted => 'D',
            DeltaChange::Renamed => 'R',
            DeltaChange::Modified => 'M',
            DeltaChange::Conflicted => 'U',
            DeltaChange::Typechange => 'T',
            DeltaChange::Ignored => '!',
            DeltaChange::Unreadable => 'X',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DeltaChange::Added => "Added",
            DeltaChange::Untracked => "Untracked",
            DeltaChange::Copied => "Copied",
            DeltaChange::Deleted => "Deleted",
            DeltaChange::Renamed => "Renamed",
            DeltaChange::Modified => "Modified",
            DeltaChange::Conflicted => "Conflicted",
            DeltaChange::Typechange => "Type changed",
            DeltaChange::Ignored => "Ignored",
            DeltaChange::Unreadable => "Unreadable",
        }
    }

    /// The file only exists on the new side.
    pub fn is_addition(self) -> bool {
        matches!(self, DeltaChange::Added | DeltaChange::Untracked | DeltaChange::Copied)
    }

    pub fn is_deletion(self) -> bool {
        self == DeltaChange::Deleted
    }

    pub fn is_conflict(self) -> bool {
        self == DeltaChange::Conflicted
    }

    /// Whether the change has an old path distinct from the new one.
    pub fn has_old_path(self) -> bool {
        matches!(self, DeltaChange::Renamed | DeltaChange::Copied)
    }
}

impl Delta {
    pub fn new(path: impl Into<PathBuf>, change: DeltaChange) -> Self {
        Self {
            path: path.into(),
            old_path: None,
            change,
        }
    }

    /// A rename or copy from `old_path`.
    pub fn moved(old_path: impl Into<PathBuf>, path: impl Into<PathBuf>, change: DeltaChange) -> Self {
        Self {
            path: path.into(),
            old_path: Some(old_path.into()),
            change,
        }
    }

    /// Last path component (the old model's `file_name`). `None` only for an empty path.
    pub fn file_name(&self) -> Option<&OsStr> {
        self.path.file_name()
    }

    /// Containing directory, or `None` for files at the repository root (the old model's `directory`, which was
    /// a string with a trailing `/`, or empty at the root).
    pub fn directory(&self) -> Option<&Path> {
        self.path.parent().filter(|p| !p.as_os_str().is_empty())
    }

    /// `old → new` for renames/copies, else the path; for display (lossy for non-UTF-8 paths).
    pub fn display_label(&self) -> String {
        match &self.old_path {
            Some(old) if old != &self.path => format!("{} → {}", old.display(), self.path.display()),
            _ => self.path.display().to_string(),
        }
    }
}

/// Added / modified / deleted file counts (stash list badges, commit summaries).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChangeCounts {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
}

impl ChangeCounts {
    /// Tallies changes. `Added`/`Untracked`/`Copied` count as added, `Deleted` as deleted, `Modified`/`Renamed`/
    /// `Typechange`/`Conflicted` as modified (the file exists on both sides); `Ignored`/`Unreadable` are not counted.
    /// The old stash code counted renames as additions.
    pub fn from_changes(changes: impl IntoIterator<Item = DeltaChange>) -> Self {
        changes.into_iter().fold(Self::default(), |mut counts, change| {
            match change {
                DeltaChange::Added | DeltaChange::Untracked | DeltaChange::Copied => counts.added += 1,
                DeltaChange::Deleted => counts.deleted += 1,
                DeltaChange::Modified | DeltaChange::Renamed | DeltaChange::Typechange | DeltaChange::Conflicted => counts.modified += 1,
                DeltaChange::Ignored | DeltaChange::Unreadable => {}
            }
            counts
        })
    }

    pub fn from_deltas<'a>(deltas: impl IntoIterator<Item = &'a Delta>) -> Self {
        Self::from_changes(deltas.into_iter().map(|d| d.change))
    }

    pub fn total(&self) -> usize {
        self.added + self.modified + self.deleted
    }
}

impl FileState {
    /// Nothing staged and nothing unstaged.
    pub fn is_clean(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty()
    }

    pub fn has_conflicts(&self) -> bool {
        self.conflicted().next().is_some()
    }

    /// Conflicted entries (libgit2 reports them on the unstaged side; both lists are checked).
    pub fn conflicted(&self) -> impl Iterator<Item = &Delta> {
        self.unstaged.iter().chain(&self.staged).filter(|d| d.change.is_conflict())
    }

    pub fn find_staged(&self, path: &Path) -> Option<&Delta> {
        self.staged.iter().find(|d| d.path == path)
    }

    pub fn find_unstaged(&self, path: &Path) -> Option<&Delta> {
        self.unstaged.iter().find(|d| d.path == path)
    }

    /// Sorts both lists by path (stable, byte order), so equal states compare equal regardless of walk order.
    pub fn sort(&mut self) {
        self.staged.sort_by(|a, b| a.path.cmp(&b.path));
        self.unstaged.sort_by(|a, b| a.path.cmp(&b.path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::round_trip;

    #[test]
    fn change_codes_are_unique() {
        let mut codes: Vec<char> = DeltaChange::ALL.iter().map(|c| c.code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), DeltaChange::ALL.len());
        assert!(DeltaChange::ALL.iter().all(|c| !c.label().is_empty()));
    }

    #[test]
    fn change_predicates() {
        assert!(DeltaChange::Untracked.is_addition() && DeltaChange::Copied.is_addition());
        assert!(!DeltaChange::Renamed.is_addition());
        assert!(DeltaChange::Deleted.is_deletion());
        assert!(DeltaChange::Conflicted.is_conflict());
        assert!(DeltaChange::Renamed.has_old_path() && !DeltaChange::Modified.has_old_path());
    }

    #[test]
    fn path_parts() {
        let nested = Delta::new("src/models/file.rs", DeltaChange::Modified);
        assert_eq!(nested.file_name(), Some(OsStr::new("file.rs")));
        assert_eq!(nested.directory(), Some(Path::new("src/models")));
        let root = Delta::new("README.md", DeltaChange::Added);
        assert_eq!(root.directory(), None);
        assert_eq!(root.file_name(), Some(OsStr::new("README.md")));
        assert_eq!(root.display_label(), "README.md");
        let renamed = Delta::moved("a/old.rs", "b/new.rs", DeltaChange::Renamed);
        assert_eq!(renamed.display_label(), "a/old.rs → b/new.rs");
        assert_eq!(Delta::new("", DeltaChange::Modified).file_name(), None);
    }

    #[test]
    fn counts() {
        let counts = ChangeCounts::from_changes(DeltaChange::ALL);
        assert_eq!(
            counts,
            ChangeCounts {
                added: 3,
                modified: 4,
                deleted: 1
            }
        );
        assert_eq!(counts.total(), 8);
        let deltas = [Delta::moved("a", "b", DeltaChange::Renamed), Delta::new("c", DeltaChange::Deleted)];
        assert_eq!(
            ChangeCounts::from_deltas(&deltas),
            ChangeCounts {
                added: 0,
                modified: 1,
                deleted: 1
            }
        );
        round_trip(&counts);
    }

    #[test]
    fn file_state_queries() {
        let mut state = FileState {
            unstaged: vec![
                Delta::new("z.txt", DeltaChange::Modified),
                Delta::new("c.txt", DeltaChange::Conflicted),
            ],
            staged: vec![Delta::new("b.txt", DeltaChange::Added), Delta::new("a.txt", DeltaChange::Deleted)],
        };
        assert!(!state.is_clean() && FileState::default().is_clean());
        assert!(state.has_conflicts());
        assert_eq!(state.conflicted().count(), 1);
        assert!(state.find_staged(Path::new("a.txt")).is_some());
        assert!(state.find_unstaged(Path::new("a.txt")).is_none());
        state.sort();
        assert_eq!(state.staged[0].path, Path::new("a.txt"));
        assert_eq!(state.unstaged[0].path, Path::new("c.txt"));
    }

    #[test]
    fn serde_round_trip() {
        for change in DeltaChange::ALL {
            round_trip(&change);
            round_trip(&Delta::new("dir/ünï code.txt", change));
        }
        let json = round_trip(&Delta::moved("old", "new", DeltaChange::Renamed));
        assert_eq!(json, r#"{"path":"new","old_path":"old","change":"renamed"}"#);
        round_trip(&FileState {
            unstaged: vec![Delta::new("a", DeltaChange::Typechange)],
            staged: vec![Delta::new("b", DeltaChange::Unreadable)],
        });
    }

    /// Non-UTF-8 paths can't be represented in JSON; serialising fails loudly instead of corrupting the path.
    #[cfg(unix)]
    #[test]
    fn non_utf8_path_fails_to_serialise() {
        use std::os::unix::ffi::OsStrExt;
        let delta = Delta::new(OsStr::from_bytes(b"bad\xff.txt"), DeltaChange::Modified);
        assert!(serde_json::to_string(&delta).is_err());
        assert_eq!(delta.file_name().map(OsStr::as_bytes), Some(&b"bad\xff.txt"[..]));
    }
}
