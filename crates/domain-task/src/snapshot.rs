//! Snapshot value types: capture reason + point-in-time task copy.

use domain_core::{RepoId, TaskId, Timestamp};
use serde::{Deserialize, Serialize};

use crate::enums::{Priority, SyncState, TaskStatus};
use crate::relation::RemoteRef;

/// Why a snapshot was captured. Only events that confirm remote alignment
/// (`Promote` / `Push` / `Pull` / `ConflictResolve`) count toward the diff
/// baseline used by dirty detection. `LocalEdit`, `PrePull`, and
/// `Rollback` write rows into the history but don't reset the baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotSource {
    /// v1 of a freshly-created task. Distinct from `LocalEdit` so the
    /// snapshot history tells you when a task came into existence vs. when
    /// it was later revised. (Previously creations also wrote `LocalEdit`,
    /// which made `version == 1` the only way to identify the creation
    /// row — fragile once flows like `sync import` start landing v1 with
    /// source `Pull`.)
    Created,
    /// A local mutation: title/body/status/etc. edit driven by the user.
    LocalEdit,
    /// First successful remote create (`promote_to_remote`).
    Promote,
    /// Successful push of a `DirtyLocal` task.
    Push,
    /// Local state captured *before* a pull overwrites it — the undo
    /// target if the user wants to revert the pull.
    PrePull,
    /// Local state after a successful pull from remote.
    Pull,
    /// Local state after a manual merge resolution.
    ConflictResolve,
    /// Local state after a rollback applied a historical snapshot.
    Rollback,
    /// Local state after `rl task link` rewired the task to a different
    /// remote (verified relink after a transfer, or arbitrary attach). The
    /// application layer is responsible for writing baseline data into the
    /// snapshot only on the verified-relink path; bare link saves with this
    /// source while leaving the task in `Conflict` for the user to resolve.
    Link,
}

impl SnapshotSource {
    /// Snapshots tagged with these sources represent a moment of remote
    /// alignment and act as the diff baseline for dirty detection.
    pub fn is_baseline(self) -> bool {
        matches!(
            self,
            SnapshotSource::Promote
                | SnapshotSource::Push
                | SnapshotSource::Pull
                | SnapshotSource::ConflictResolve
                | SnapshotSource::Link
        )
    }
}

/// A point-in-time copy of a task's remote-observable state plus the
/// reason it was captured. Append-only — the sequence of snapshots for a
/// task is its full edit history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSnapshot {
    pub task_id: TaskId,
    pub version: u64,
    pub title: String,
    pub body: String,
    pub status: TaskStatus,
    pub sync_state: SyncState,
    pub priority: Priority,
    pub assignees: Vec<String>,
    pub remote: Option<RemoteRef>,
    /// The task's binding at the time of the snapshot. Captured so that
    /// `rl task rollback` can restore the binding pointer too — link /
    /// `--relink` operations mutate `repo_id`, and rolling content back
    /// without rolling the binding back would leave the task pointing at a
    /// foreign repo's remote_id.
    pub repo_id: Option<RepoId>,
    /// Whether the snapshot's `repo_id` was actually recorded at write time
    /// (vs. NULL-backfilled by the migration that introduced the column).
    /// Rollback uses this to tell "the task was intentionally unbound at v3"
    /// (recorded = true, repo_id = None → clear the binding) apart from "we
    /// don't know what v3's binding was" (recorded = false → preserve the
    /// current binding). Always `true` for snapshots written after the
    /// column landed.
    pub repo_id_recorded: bool,
    /// The task's **filing repo** (RFC 0002 #118) at the time of the snapshot —
    /// where its backing GitHub issue is filed. History / audit only: captured
    /// so promote / push / pull / conflict-resolve / link snapshots carry the
    /// resolved filing repo. Deliberately EXCLUDED from dirty detection
    /// (`Task::reconcile_dirty_against_baseline` never reads it) and NOT
    /// restored on rollback — the filing repo of a remote-backed task is
    /// immutable post-promote and D6 / #120 keys remote identity on it, so
    /// `TaskService::rollback` leaves the live `filing_repo_id` untouched.
    /// Because rollback never restores it, there is no rollback ambiguity to
    /// disambiguate, so there is NO `filing_repo_id_recorded` companion flag
    /// (unlike `repo_id_recorded`). Pre-column snapshot rows read back as
    /// `None`.
    pub filing_repo_id: Option<RepoId>,
    pub source: SnapshotSource,
    pub captured_at: Timestamp,
}

impl TaskSnapshot {
    /// Whether this snapshot represents a moment of remote alignment that
    /// dirty detection should diff against. Stricter than
    /// [`SnapshotSource::is_baseline`]: a `Link` snapshot is baseline-eligible
    /// only when the task ended up `Synced` (verified relink); a bare link
    /// flips to `Conflict` and explicitly does NOT establish alignment, so
    /// loading that row as the baseline would mis-anchor diff detection.
    pub fn is_baseline(&self) -> bool {
        self.source.is_baseline()
            && !(self.source == SnapshotSource::Link && self.sync_state == SyncState::Conflict)
    }
}
