//! Persisted and shared models (serde round-trip tests keep their shapes stable).

mod branch;
mod cache;
mod commit;
mod conflict;
mod file;
mod head;
mod outcome;
mod progress;
mod reference;
mod repository;
mod settings;
mod stash;
mod status;
mod tag;
mod worktree;

pub use branch::{Branch, BranchKind};
pub use cache::{CommitRow, RefTip, RefsFingerprint};
pub use commit::{Commit, CommitBody, CommitWrapper, ResetMode};
pub use conflict::{ConflictChunk, ConflictSide, ConflictSides, ConflictType, DeletedMergeAction, MergeConflict};
pub use file::{ChangeCounts, Delta, DeltaChange, FileState};
pub use head::Head;
pub use outcome::{AbortMergeOutcome, FetchSummary, MergeOutcome, PullOutcome, PushOutcome, RefUpdate, RemoteFetch, StashApplyOutcome};
pub use progress::{RemoteOp, RemoteProgress};
pub use reference::{Reference, ReferenceKind, shorten_refname, split_remote_branch};
pub use repository::{OpenTabs, Repository};
pub use settings::{
    AutoCrlf, CommitSignature, DEFAULT_FETCH_INTERVAL_SECS, DEFAULT_MAX_COMMITS, DiffViewMode, GraphOptions, MIN_FETCH_INTERVAL_SECS,
    RailPosition, Settings, SpeedSettings, Theme,
};
pub use stash::{Stash, StashDetails};
pub use status::{RepoOperation, RepoStatus};
pub use tag::{Tag, TagAnnotation};
pub use worktree::{Worktree, WorktreeBranch};
