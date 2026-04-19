pub mod basket_ops;
pub mod blame;
pub mod find_fix_ops;
pub mod reword_ops;
pub mod cli;
pub mod diff;
pub mod graph;
pub mod hunk_staging;
pub mod jobs;
pub mod lfs;
pub mod message_lint;
pub mod ops;
pub mod repo;
pub mod split_ops;

pub use basket_ops::{
    revert_to_working_tree, squash_basket_into_one, RevertOutcome, SquashOutcome,
};
pub use find_fix_ops::{
    apply as find_fix_apply, scan as find_fix_scan, ApplyOutcome as FindFixApplyOutcome, ApplyPlan
    as FindFixApplyPlan, CommitMatch as FindFixCommitMatch, ScanResult as FindFixScanResult,
    WorkingTreeMatch as FindFixWorkingTreeMatch,
};
pub use message_lint::{
    auto_fix as lint_auto_fix, lint as lint_message, load_rules as load_message_lint_rules,
    rules_file_path as message_lint_rules_path, Finding as LintFinding, RulesFile as LintRulesFile,
    Scope as LintScope, Severity as LintSeverity,
};
pub use reword_ops::{reword_commit, RewordOutcome};
pub use split_ops::{
    discover_hunks, split_commit, DiscoveredHunk, HunkRef, SplitOutcome, SplitPart, SplitPlan,
};
pub use blame::{blame_file, BlameCommit, BlameLine, BlameResult};
pub use cli::{
    classify_git_error, probe_git_capability, recent_git_log, GitCapability, GitErrorKind,
};
pub use diff::{
    diff_for_commit, diff_for_commit_in, diff_text_for_working_entry, diff_text_staged_only,
    diff_text_unstaged_only, file_diff_for_working_entry, intra_line_diff, DeltaStatus, DiffLine,
    FileDiff, FileKind, Hunk, IntraLineDiff, IntraLineSpan, LineKind, RepoDiff,
};
pub use hunk_staging::{
    discard_hunk, hunk_staging_block_reason, sanitize_selection, stage_hunk, unstage_hunk,
    DiffSide, HunkSelectionState, HunkSelector,
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
