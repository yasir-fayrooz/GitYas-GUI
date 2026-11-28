use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Oid;
use crate::models::{Commit, Delta};

/// One commit that changed a file (`Engine::file_history`), listed newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHistoryEntry {
    pub commit: Commit,
    /// The file as this commit changed it, under its path in that commit (`old_path` is set where it was renamed),
    /// so it can be shown with `DiffTarget::Commit(commit.id)`.
    pub file: Delta,
}

/// Consecutive lines last changed by the same commit (`Engine::file_blame`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameHunk {
    /// `None` for lines that are not committed yet (working-tree and staged blame).
    pub commit: Option<Oid>,
    pub author_name: String,
    /// Seconds since the Unix epoch (0 for uncommitted lines).
    pub author_time: i64,
    /// First line of the commit's message (empty for uncommitted lines).
    pub summary: String,
    /// The file's path in that commit (differs from the blamed path where the file was renamed since).
    pub path: PathBuf,
    /// First line of the hunk, 0-based, in [`FileBlame::text`].
    pub start: usize,
    /// Number of lines.
    pub lines: usize,
}

impl BlameHunk {
    pub fn is_committed(&self) -> bool {
        self.commit.is_some()
    }
}

/// A file's text with, for every line, the commit that last changed it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileBlame {
    /// The blamed version of the file, decoded as UTF-8 (lossily if `lossy`).
    pub text: String,
    pub lossy: bool,
    /// In line order; together they cover every line of `text`.
    pub hunks: Vec<BlameHunk>,
}

impl FileBlame {
    /// The hunk that covers line `line` (0-based).
    pub fn hunk_at(&self, line: usize) -> Option<&BlameHunk> {
        let at = self.hunks.partition_point(|h| h.start + h.lines <= line);
        self.hunks.get(at).filter(|h| h.start <= line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{oid, round_trip};

    fn hunk(start: usize, lines: usize) -> BlameHunk {
        BlameHunk {
            commit: Some(oid(1)),
            author_name: "Ada".into(),
            author_time: 1_700_000_000,
            summary: "feat: x".into(),
            path: "src/lib.rs".into(),
            start,
            lines,
        }
    }

    #[test]
    fn hunk_lookup() {
        let blame = FileBlame {
            text: "a\nb\nc\nd\n".into(),
            lossy: false,
            hunks: vec![hunk(0, 1), hunk(1, 2), hunk(3, 1)],
        };
        assert_eq!(blame.hunk_at(0).map(|h| h.start), Some(0));
        assert_eq!(blame.hunk_at(2).map(|h| h.start), Some(1));
        assert_eq!(blame.hunk_at(3).map(|h| h.start), Some(3));
        assert_eq!(blame.hunk_at(4), None);
        round_trip(&blame);
    }
}
