//! Standalone serde enums with no behaviour.

use serde::{Deserialize, Serialize};

/// Where the task is in the human workflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Created but no one has started it.
    Open,
    /// Actively being worked on.
    InProgress,
    /// Stuck on an external dependency.
    Blocked,
    /// Work is complete. Distinct from `Archived` — done tasks stay
    /// visible in dashboards; archived ones are out of sight.
    Done,
    /// Terminal — dropped, deferred indefinitely, or post-done cleanup.
    Archived,
}

/// How the local copy of the task relates to its remote counterpart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// Never pushed; lives only in the local SQLite store.
    LocalOnly,
    /// Marked for sync, not yet pushed.
    Staged,
    /// Local matches the last known remote snapshot.
    Synced,
    /// Local has uncommitted edits since the last successful sync.
    DirtyLocal,
    /// Remote has changed since the last successful sync.
    DirtyRemote,
    /// Both sides diverged — needs human resolution.
    Conflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    P0,
    P1,
    P2,
    P3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    BlockedBy,
    Blocks,
    DependsOn,
    Duplicates,
    ParentOf,
    ChildOf,
    RelatedTo,
}
