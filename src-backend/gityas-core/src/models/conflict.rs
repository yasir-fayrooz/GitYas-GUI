use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::diff::DiffKind;

/// Merge-conflict contents for one path (semantics: `gityas_git::merge`).
///
/// Text fields are empty for a side that doesn't exist (deleted, or added on one side only) and for sides whose
/// [`ConflictSide::kind`] isn't `Text` (binary, symlink, submodule, non-UTF-8, too large): check [`Self::sides`]
/// to choose between the text merge editor and the take-ours / take-theirs / delete actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeConflict {
    /// The merge base version (`""` for add/add conflicts).
    pub ancestor: String,
    pub ours: String,
    pub theirs: String,
    /// Auto-merged template the editor edits: non-conflicting changes applied, base lines inside each conflict.
    /// Empty when a side is deleted.
    pub merged: String,
    pub chunks: Vec<ConflictChunk>,
    pub sides: ConflictSides,
    pub ours_deleted: bool,
    pub theirs_deleted: bool,
    pub auto_resolved: bool,
}

/// What one index stage of a conflict holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictSide {
    /// Git file mode (`0o100644`, `0o100755`, `0o120000` symlink, `0o160000` submodule).
    pub mode: u32,
    pub bytes: u64,
    /// `Text`, `Binary`, `NonUtf8` or `TooLarge`. Symlinks and submodules are reported as `Binary` (not editable as text).
    pub kind: DiffKind,
}

/// The three stages of a conflicted path; `None` = that version does not exist (deleted, or added on one side).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictSides {
    pub ancestor: Option<ConflictSide>,
    pub ours: Option<ConflictSide>,
    pub theirs: Option<ConflictSide>,
}

impl ConflictSides {
    /// True when every present side can be shown in the text merge editor.
    pub fn is_text(&self) -> bool {
        [&self.ancestor, &self.ours, &self.theirs]
            .into_iter()
            .flatten()
            .all(|s| s.kind == DiffKind::Text)
    }
}

/// Kind of a conflict chunk. The text merge engine currently only produces [`ConflictType::Both`] (git's
/// semantics: one-sided changes are merged into [`MergeConflict::merged`], not reported as chunks); deletions are
/// expressed by [`MergeConflict::ours_deleted`] / [`MergeConflict::theirs_deleted`]. The other variants are kept
/// for a future chunk-by-chunk view of auto-merged changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictType {
    /// Both sides modified.
    Both,
    OursOnly,
    TheirsOnly,
    /// One side deleted, the other modified.
    Deletion,
}

/// Line ranges (0-based, end-exclusive) of one chunk in each version and in [`MergeConflict::merged`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictChunk {
    pub conflict_type: ConflictType,
    pub ours_start: usize,
    pub ours_end: usize,
    pub theirs_start: usize,
    pub theirs_end: usize,
    pub ancestor_start: usize,
    pub ancestor_end: usize,
    pub merged_start: usize,
    pub merged_end: usize,
}

/// Resolution for delete/modify conflicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletedMergeAction {
    TakeOurs,
    TakeTheirs,
    TakeBase,
    DeleteFile,
}

impl ConflictType {
    /// `Both` and `Deletion` need a decision from the user; one-sided changes merge automatically.
    pub fn needs_resolution(self) -> bool {
        matches!(self, ConflictType::Both | ConflictType::Deletion)
    }
}

impl ConflictChunk {
    pub fn ours_range(&self) -> Range<usize> {
        self.ours_start..self.ours_end
    }

    pub fn theirs_range(&self) -> Range<usize> {
        self.theirs_start..self.theirs_end
    }

    pub fn ancestor_range(&self) -> Range<usize> {
        self.ancestor_start..self.ancestor_end
    }

    pub fn merged_range(&self) -> Range<usize> {
        self.merged_start..self.merged_end
    }

    /// Every range has `start <= end` (a chunk built from bad indices would make the editor slice out of bounds).
    pub fn is_well_formed(&self) -> bool {
        [self.ours_range(), self.theirs_range(), self.ancestor_range(), self.merged_range()]
            .iter()
            .all(|r| r.start <= r.end)
    }
}

impl MergeConflict {
    /// A delete/modify conflict: resolved with a [`DeletedMergeAction`] instead of chunk-by-chunk.
    pub fn is_delete_conflict(&self) -> bool {
        self.ours_deleted || self.theirs_deleted
    }

    /// Chunks the user still has to decide.
    pub fn unresolved_chunks(&self) -> impl Iterator<Item = &ConflictChunk> {
        self.chunks.iter().filter(|c| c.conflict_type.needs_resolution())
    }

    /// Nothing left for the user to decide.
    pub fn is_resolved(&self) -> bool {
        self.auto_resolved || (!self.is_delete_conflict() && self.unresolved_chunks().next().is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::round_trip;

    fn chunk(conflict_type: ConflictType) -> ConflictChunk {
        ConflictChunk {
            conflict_type,
            ours_start: 2,
            ours_end: 4,
            theirs_start: 2,
            theirs_end: 5,
            ancestor_start: 2,
            ancestor_end: 3,
            merged_start: 2,
            merged_end: 3,
        }
    }

    fn conflict(chunks: Vec<ConflictChunk>) -> MergeConflict {
        MergeConflict {
            ancestor: "a\nb\n".into(),
            ours: "a\nB\n".into(),
            theirs: "a\nb2\n".into(),
            merged: "a\nb\n".into(),
            chunks,
            sides: ConflictSides::default(),
            ours_deleted: false,
            theirs_deleted: false,
            auto_resolved: false,
        }
    }

    #[test]
    fn chunk_ranges() {
        let c = chunk(ConflictType::Both);
        assert_eq!(c.ours_range(), 2..4);
        assert_eq!(c.theirs_range(), 2..5);
        assert_eq!(c.ancestor_range(), 2..3);
        assert!(c.is_well_formed());
        assert!(!ConflictChunk { ours_end: 1, ..c }.is_well_formed());
    }

    #[test]
    fn resolution_state() {
        assert!(conflict(vec![]).is_resolved());
        assert!(conflict(vec![chunk(ConflictType::OursOnly), chunk(ConflictType::TheirsOnly)]).is_resolved());
        let open = conflict(vec![chunk(ConflictType::OursOnly), chunk(ConflictType::Both)]);
        assert_eq!(open.unresolved_chunks().count(), 1);
        assert!(!open.is_resolved());
        let deleted = MergeConflict {
            theirs_deleted: true,
            ..conflict(vec![])
        };
        assert!(deleted.is_delete_conflict() && !deleted.is_resolved());
        let auto = MergeConflict {
            auto_resolved: true,
            ..deleted
        };
        assert!(auto.is_resolved());
    }

    #[test]
    fn serde_round_trip() {
        for t in [
            ConflictType::Both,
            ConflictType::OursOnly,
            ConflictType::TheirsOnly,
            ConflictType::Deletion,
        ] {
            round_trip(&t);
            round_trip(&chunk(t));
        }
        round_trip(&conflict(vec![chunk(ConflictType::Both), chunk(ConflictType::Deletion)]));
        for a in [
            DeletedMergeAction::TakeOurs,
            DeletedMergeAction::TakeTheirs,
            DeletedMergeAction::TakeBase,
            DeletedMergeAction::DeleteFile,
        ] {
            round_trip(&a);
        }
        assert_eq!(round_trip(&DeletedMergeAction::DeleteFile), r#""delete_file""#);
    }

    #[test]
    fn sides_is_text_ignores_missing_sides() {
        let side = |kind| {
            Some(ConflictSide {
                mode: 0o100_644,
                bytes: 3,
                kind,
            })
        };
        let mut sides = ConflictSides {
            ancestor: None,
            ours: side(DiffKind::Text),
            theirs: side(DiffKind::Text),
        };
        assert!(sides.is_text());
        sides.theirs = side(DiffKind::Binary);
        assert!(!sides.is_text());
        round_trip(&MergeConflict {
            sides,
            ..conflict(vec![chunk(ConflictType::Both)])
        });
    }
}
