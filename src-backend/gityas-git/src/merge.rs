//! Three-way text merge and index-conflict read / resolve.
//!
//! * [`engine`]: pure line merge, a port of git's `xdl_do_merge` (EAGER level = `git merge-file --diff3`) on top
//!   of libgit2's xdiff. Used for conflict markers ([`render_diff3`]) and as the editor layout's fallback.
//! * [`editor`]: the merge editor's chunk layout: an O(NP) diff + diff3
//!   grouping, which the old app's chunker (`misc/gityas_myers.rs` + `misc/gityas_merger.rs`) approximated, so
//!   chunk boundaries match the Tauri app and its fixture suite (`tests/fixtures/merge`).
//! * [`conflict`]: `get_merge_conflict`, `resolve_conflict`, `resolve_conflict_with_action` on a repository.
//!
//! Line ranges are 0-based and end-exclusive everywhere. A line keeps its terminator (`split_inclusive('\n')`), except
//! inside [`editor`], which counts `split('\n')` pieces like the old app (see its docs).

pub mod conflict;
pub mod editor;
pub mod engine;

pub use conflict::{
    ConflictSide, ConflictSides, DEFAULT_MAX_CONFLICT_BYTES, conflict_from_texts, conflict_sides, get_merge_conflict, mark_resolved,
    resolve_conflict, resolve_conflict_with_action,
};
pub use editor::{EditorChunk, EditorMerge, editor_chunks};
pub use engine::{TextConflict, TextMerge, merge_texts, render_diff3, split_lines};
