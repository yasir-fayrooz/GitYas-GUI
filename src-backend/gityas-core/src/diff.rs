//! One structured diff that drives both display and hunk staging.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::Oid;

/// What to diff. Only `WorkingTree` is mutable (stage/unstage/discard).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffTarget {
    /// `staged: true` = index vs HEAD, `false` = working tree vs index.
    WorkingTree {
        staged: bool,
    },
    /// Commit vs its first parent (root commits vs the empty tree).
    Commit(Oid),
    Stash(Oid),
}

impl DiffTarget {
    pub fn is_read_only(&self) -> bool {
        !matches!(self, DiffTarget::WorkingTree { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    Text,
    Binary,
    /// Over the configured size cap; `bytes` is the larger side.
    TooLarge {
        bytes: u64,
    },
    /// Not valid UTF-8: `old_text`/`new_text` are empty, but hunks are present with lossily decoded lines (the UI
    /// shows an explanation and may offer them read-only).
    NonUtf8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    /// Full sides, for Inline / Split views. Empty for added/deleted sides and non-`Text` kinds.
    pub old_text: Arc<str>,
    pub new_text: Arc<str>,
    /// git2 Patch hunks with 3 lines of context.
    pub hunks: Vec<Hunk>,
    pub kind: DiffKind,
    /// Git file modes of each side (`0o100644`, `0o100755`, `0o120000` symlink, `0o160000` submodule); `None` for
    /// a side that doesn't exist. Lets the UI show mode-only changes and typechanges, which have no text hunks.
    pub old_mode: Option<u32>,
    pub new_mode: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub id: HunkRef,
    /// The `@@ -a,b +c,d @@ …` line.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// Identifies a hunk for stage/unstage/discard. `content_hash` covers the hunk's lines, so staging a hunk that
/// changed since display fails with `GitYasError::StaleHunk` instead of staging different content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HunkRef {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub content_hash: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineOrigin {
    Context,
    Add,
    Del,
    /// The `\ No newline at end of file` marker.
    NoNewlineAtEof,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub origin: LineOrigin,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
    /// Line content without the line terminator (`\n` or `\r\n`).
    pub text: String,
}

impl DiffKind {
    /// Only `Text` diffs have lines to show and hunks to stage.
    pub fn is_text(self) -> bool {
        self == DiffKind::Text
    }

    /// User-facing explanation shown instead of the diff for non-text kinds; `None` for `Text`.
    pub fn placeholder_message(self) -> Option<String> {
        match self {
            DiffKind::Text => None,
            DiffKind::Binary => Some("Binary file not shown".into()),
            DiffKind::TooLarge { bytes } => Some(format!("File too large to display ({bytes} bytes)")),
            DiffKind::NonUtf8 => Some("File is not valid UTF-8 and can't be displayed".into()),
        }
    }
}

impl FileDiff {
    /// A diff with no sides and no hunks, for `Binary` / `TooLarge` / `NonUtf8` (or an empty `Text` change such as
    /// a mode-only change).
    pub fn without_content(kind: DiffKind) -> Self {
        Self {
            old_text: Arc::from(""),
            new_text: Arc::from(""),
            hunks: Vec::new(),
            kind,
            old_mode: None,
            new_mode: None,
        }
    }

    /// Both sides exist and their modes differ (e.g. `chmod +x`, file ↔ symlink).
    pub fn mode_changed(&self) -> bool {
        matches!((self.old_mode, self.new_mode), (Some(a), Some(b)) if a != b)
    }

    pub fn has_hunks(&self) -> bool {
        !self.hunks.is_empty()
    }

    /// `(added, deleted)` line counts over all hunks.
    pub fn line_stats(&self) -> (usize, usize) {
        self.hunks.iter().fold((0, 0), |(add, del), hunk| {
            let (a, d) = hunk.line_stats();
            (add + a, del + d)
        })
    }

    /// The hunk with this id (including its content hash), if the diff still contains it.
    pub fn find_hunk(&self, id: &HunkRef) -> Option<&Hunk> {
        self.hunks.iter().find(|h| &h.id == id)
    }
}

impl Hunk {
    /// `(added, deleted)` line counts.
    pub fn line_stats(&self) -> (usize, usize) {
        self.lines.iter().fold((0, 0), |(add, del), line| match line.origin {
            LineOrigin::Add => (add + 1, del),
            LineOrigin::Del => (add, del + 1),
            LineOrigin::Context | LineOrigin::NoNewlineAtEof => (add, del),
        })
    }
}

impl HunkRef {
    /// Same position, ignoring `content_hash` (to tell "moved/changed" apart from "gone" when a hunk is stale).
    pub fn same_range(&self, other: &HunkRef) -> bool {
        (self.old_start, self.old_lines, self.new_start, self.new_lines)
            == (other.old_start, other.old_lines, other.new_start, other.new_lines)
    }
}

impl LineOrigin {
    /// Unified-diff prefix character.
    pub fn prefix(self) -> char {
        match self {
            LineOrigin::Context => ' ',
            LineOrigin::Add => '+',
            LineOrigin::Del => '-',
            LineOrigin::NoNewlineAtEof => '\\',
        }
    }

    pub fn is_change(self) -> bool {
        matches!(self, LineOrigin::Add | LineOrigin::Del)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{oid, round_trip};

    fn hunk_ref(hash: u64) -> HunkRef {
        HunkRef {
            old_start: 1,
            old_lines: 2,
            new_start: 1,
            new_lines: 3,
            content_hash: hash,
        }
    }

    fn line(origin: LineOrigin, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
        DiffLine {
            origin,
            old_lineno: old,
            new_lineno: new,
            text: text.into(),
        }
    }

    fn sample() -> FileDiff {
        FileDiff {
            old_text: Arc::from("a\nb"),
            new_text: Arc::from("a\nc\nd"),
            hunks: vec![Hunk {
                id: hunk_ref(u64::MAX),
                header: "@@ -1,2 +1,3 @@".into(),
                lines: vec![
                    line(LineOrigin::Context, Some(1), Some(1), "a"),
                    line(LineOrigin::Del, Some(2), None, "b"),
                    line(LineOrigin::NoNewlineAtEof, None, None, ""),
                    line(LineOrigin::Add, None, Some(2), "c"),
                    line(LineOrigin::Add, None, Some(3), "d"),
                    line(LineOrigin::NoNewlineAtEof, None, None, ""),
                ],
            }],
            kind: DiffKind::Text,
            old_mode: Some(0o100_644),
            new_mode: Some(0o100_644),
        }
    }

    #[test]
    fn stats_and_lookup() {
        let diff = sample();
        assert!(diff.has_hunks());
        assert_eq!(diff.line_stats(), (2, 1));
        assert!(diff.find_hunk(&hunk_ref(u64::MAX)).is_some());
        assert!(diff.find_hunk(&hunk_ref(0)).is_none());
        assert!(hunk_ref(1).same_range(&hunk_ref(2)));
        let moved = HunkRef {
            new_start: 9,
            ..hunk_ref(1)
        };
        assert!(!moved.same_range(&hunk_ref(1)));
    }

    #[test]
    fn kinds_and_placeholders() {
        assert!(DiffKind::Text.is_text() && DiffKind::Text.placeholder_message().is_none());
        for kind in [DiffKind::Binary, DiffKind::TooLarge { bytes: 10 }, DiffKind::NonUtf8] {
            assert!(!kind.is_text());
            assert!(kind.placeholder_message().is_some());
            let empty = FileDiff::without_content(kind);
            assert!(!empty.has_hunks() && empty.old_text.is_empty() && empty.new_text.is_empty());
            assert_eq!(empty.line_stats(), (0, 0));
        }
        assert!(
            DiffKind::TooLarge { bytes: 42 }
                .placeholder_message()
                .is_some_and(|m| m.contains("42"))
        );
    }

    #[test]
    fn line_origins() {
        let prefixes: String = [LineOrigin::Context, LineOrigin::Add, LineOrigin::Del, LineOrigin::NoNewlineAtEof]
            .iter()
            .map(|o| o.prefix())
            .collect();
        assert_eq!(prefixes, " +-\\");
        assert!(LineOrigin::Add.is_change() && !LineOrigin::NoNewlineAtEof.is_change());
    }

    #[test]
    fn targets() {
        assert!(!DiffTarget::WorkingTree { staged: true }.is_read_only());
        assert!(DiffTarget::Commit(oid(1)).is_read_only());
        assert!(DiffTarget::Stash(oid(1)).is_read_only());
    }

    #[test]
    fn serde_round_trip() {
        round_trip(&sample());
        for kind in [
            DiffKind::Text,
            DiffKind::Binary,
            DiffKind::TooLarge { bytes: u64::MAX },
            DiffKind::NonUtf8,
        ] {
            round_trip(&FileDiff::without_content(kind));
        }
        for target in [
            DiffTarget::WorkingTree { staged: false },
            DiffTarget::WorkingTree { staged: true },
            DiffTarget::Commit(oid(1)),
            DiffTarget::Stash(oid(2)),
        ] {
            round_trip(&target);
        }
        assert_eq!(
            round_trip(&DiffTarget::WorkingTree { staged: true }),
            r#"{"working_tree":{"staged":true}}"#
        );
        assert_eq!(round_trip(&DiffKind::NonUtf8), r#""non_utf8""#);
        assert_eq!(round_trip(&LineOrigin::NoNewlineAtEof), r#""no_newline_at_eof""#);
        // u64 hashes above i64::MAX survive JSON.
        assert!(round_trip(&hunk_ref(u64::MAX)).contains("18446744073709551615"));
    }

    #[test]
    fn mode_changed_needs_both_sides() {
        let mut d = sample();
        assert!(!d.mode_changed());
        d.new_mode = Some(0o100_755);
        assert!(d.mode_changed());
        d.old_mode = None;
        assert!(!d.mode_changed(), "added file is not a mode change");
    }
}
