pub mod basket_ops;
pub mod blame;
pub mod cli;
pub mod diff;
pub mod graph;
pub mod hunk_staging;
pub mod jobs;
pub mod lfs;
pub mod ops;
pub mod repo;

pub use basket_ops::{
    revert_to_working_tree, squash_basket_into_one, RevertOutcome, SquashOutcome,
};
pub use blame::{blame_file, BlameCommit, BlameLine, BlameResult};
pub use cli::{
    classify_git_error, probe_git_capability, recent_git_log, GitCapability, GitErrorKind,
};
pub use diff::{
    diff_for_commit, diff_for_commit_in, diff_text_for_working_entry, diff_text_staged_only,
    diff_text_unstaged_only, file_diff_for_working_entry, DeltaStatus, DiffLine, FileDiff,
    FileKind, Hunk, LineKind, RepoDiff,
};
pub use hunk_staging::{
    discard_hunk, hunk_staging_block_reason, sanitize_selection, stage_hunk, unstage_hunk,
    HunkSelector,
};
pub use graph::{CommitGraph, GraphRow, GraphScope, RefKind, RefLabel};
pub use jobs::{GitJob, GitJobKind, JobProgress, PullStrategy};
pub use lfs::{LfsCandidate, LfsScanResult};
pub use ops::{EntryKind, StashEntry, StatusEntry};
pub use repo::{
    BisectStatus, BranchInfo, ConflictBlob, ConflictChoice, ConflictEntry, CountObjectsSummary,
    LinearCommit, ReflogEntrySummary, RemoteInfo, Repo, RepoState, SparseCheckoutStatus,
    SubmoduleEntry, SubmoduleState, WorktreeInfo,
};
