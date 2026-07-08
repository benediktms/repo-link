//! Snapshot value types: capture reason + point-in-time task copy.

use domain_core::{RepoId, TaskId, Timestamp};
use serde::{Deserialize, Serialize};

use crate::enums::{Lifecycle, Priority, SyncState};
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
    /// Local state after `rl repo doctor --repair` re-pointed a task's
    /// `filing_repo_id` to a live binding (rpl-sv2 / RFC 0002 D2 repair).
    /// Baseline-eligible because the doctor is an authoritative user action:
    /// the recorded value is no longer dangling, so subsequent pull should
    /// NOT fire a phantom drift on the new (now-correct) canonical.
    FilingRepoRepair,
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
                | SnapshotSource::FilingRepoRepair
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
    /// The lifecycle axis at capture time — a single [`Lifecycle`] value
    /// (RFC 0004 D1), replacing the old `TaskStatus`.
    pub lifecycle: Lifecycle,
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
    /// only when the task ended up `Synced` (the verified-relink path). A bare
    /// link flips to `Conflict` and explicitly does NOT establish alignment,
    /// and no other `sync_state` is reachable for a `Link` snapshot (see
    /// `SyncService::link`), so gate strictly on `Synced` rather than the
    /// looser "anything but `Conflict`" — loading a non-aligned row as the
    /// baseline would mis-anchor diff detection.
    pub fn is_baseline(&self) -> bool {
        if self.source == SnapshotSource::Link {
            return self.sync_state == SyncState::Synced;
        }
        self.source.is_baseline()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire pinning which sources are baseline-eligible (rpl-sv2). A new
    /// snapshot source MUST be a deliberate addition to this set; the
    /// alternative (default-ineligible) is wrong because the doctor
    /// re-point relies on `FilingRepoRepair` rebaselining the task so the
    /// next `sync pull` doesn't fire phantom drift.
    #[test]
    fn filing_repo_repair_is_baseline_eligible() {
        assert!(
            SnapshotSource::FilingRepoRepair.is_baseline(),
            "FilingRepoRepair must be baseline-eligible — see rpl-sv2"
        );
    }

    fn snap(source: SnapshotSource, sync_state: SyncState) -> TaskSnapshot {
        TaskSnapshot {
            task_id: TaskId::new(),
            version: 1,
            title: String::new(),
            body: String::new(),
            lifecycle: Lifecycle::Open,
            sync_state,
            priority: Priority::P3,
            assignees: vec![],
            remote: None,
            repo_id: None,
            repo_id_recorded: true,
            filing_repo_id: None,
            source,
            captured_at: Timestamp::now(),
        }
    }

    /// A `Link` snapshot is a baseline ONLY when the relink verified and left
    /// the task `Synced`; every other state (including the reachable bare-link
    /// `Conflict`) must be rejected. Regression guard for the old predicate,
    /// which admitted `Link` in every state but `Conflict`.
    #[test]
    fn link_snapshot_is_baseline_only_when_synced() {
        assert!(snap(SnapshotSource::Link, SyncState::Synced).is_baseline());
        for other in [
            SyncState::Conflict,
            SyncState::DirtyLocal,
            SyncState::DirtyRemote,
            SyncState::Staged,
            SyncState::LocalOnly,
        ] {
            assert!(
                !snap(SnapshotSource::Link, other).is_baseline(),
                "Link snapshot in {other:?} must NOT be a baseline"
            );
        }
    }

    /// Non-`Link` baseline sources are state-independent — their eligibility
    /// is decided purely by [`SnapshotSource::is_baseline`], so even a
    /// `Conflict` sync_state does not demote them.
    #[test]
    fn non_link_baseline_sources_ignore_sync_state() {
        for source in [
            SnapshotSource::Promote,
            SnapshotSource::Push,
            SnapshotSource::Pull,
            SnapshotSource::ConflictResolve,
            SnapshotSource::FilingRepoRepair,
        ] {
            assert!(
                snap(source, SyncState::Conflict).is_baseline(),
                "{source:?} must be a baseline regardless of sync_state"
            );
        }
        for source in [
            SnapshotSource::Created,
            SnapshotSource::LocalEdit,
            SnapshotSource::PrePull,
            SnapshotSource::Rollback,
        ] {
            assert!(
                !snap(source, SyncState::Synced).is_baseline(),
                "{source:?} is not a baseline source"
            );
        }
    }
}
