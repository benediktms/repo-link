//! Pure reconciliation rules over [`SyncState`]. No I/O.
//!
//! Decoupled from `TaskStatus`: `decide` only inspects sync state. The
//! caller is responsible for filtering out tasks whose status (archived,
//! blocked, etc.) makes them ineligible for sync.

use domain_task::SyncState;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncPolicy {
    LocalWins,
    RemoteWins,
    ManualMerge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncDecision {
    Noop,
    PushLocal,
    PullRemote,
    RequireManualMerge,
}

/// Why a task is in conflict. **These variants are not yet persisted on the
/// `Task` aggregate** — a conflict is recorded only as `SyncState::Conflict`
/// (which is what `rl query drift` surfaces). The drainer's `ApplyDisposition`
/// carries a kind so per-arm tripwires and log lines can name the disagreement,
/// but the kind is dropped at the `mark_conflicted()` transition; wiring a
/// conflict-reason column is a future RFC.
///
/// There is deliberately **no** "local lifecycle vs. remote open/closed"
/// variant. RFC 0004 D1 collapsed the 5-state `TaskStatus` so `is_open` is the
/// 1:1 inverse of the REST `closed` bit, and pull now folds the open/closed bit
/// into the inbound mirror set (a local-vs-remote flip is handled by the generic
/// `decide()` → `RequireManualMerge` path). The old `StatusMismatch` variant
/// that modelled that case was removed; see RFC 0004 D1 + RFC 0003 §6 OQ5 for
/// the reasoning so it is not re-derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    LocalEditedRemoteEdited,
    RemoteDeletedLocalEdited,
    LocalDeletedRemoteEdited,
    AssigneeMismatch,
    /// A `SetProjectStatus` push whose response confirms a different
    /// `option_id` than was sent (the drainer reads back the applied
    /// single-select value, per RFC 0004 D5).
    ProjectStatusMismatch,
    RelationMismatch,
    TargetRemapped,
}

/// Decide what to do for a single task given its sync state, whether the
/// remote snapshot is known-dirty, and the configured policy.
pub fn decide(sync: SyncState, remote_dirty: bool, policy: SyncPolicy) -> SyncDecision {
    match sync {
        SyncState::LocalOnly => SyncDecision::Noop,
        SyncState::Staged => SyncDecision::PushLocal,
        SyncState::DirtyLocal if remote_dirty => match policy {
            SyncPolicy::LocalWins => SyncDecision::PushLocal,
            SyncPolicy::RemoteWins => SyncDecision::PullRemote,
            SyncPolicy::ManualMerge => SyncDecision::RequireManualMerge,
        },
        SyncState::DirtyLocal => SyncDecision::PushLocal,
        SyncState::Synced if remote_dirty => SyncDecision::PullRemote,
        SyncState::Synced => SyncDecision::Noop,
        SyncState::DirtyRemote => SyncDecision::PullRemote,
        SyncState::Conflict => match policy {
            SyncPolicy::LocalWins => SyncDecision::PushLocal,
            SyncPolicy::RemoteWins => SyncDecision::PullRemote,
            SyncPolicy::ManualMerge => SyncDecision::RequireManualMerge,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_only_is_never_synced() {
        assert_eq!(
            decide(SyncState::LocalOnly, true, SyncPolicy::ManualMerge),
            SyncDecision::Noop
        );
    }

    #[test]
    fn staged_pushes_regardless_of_remote() {
        assert_eq!(
            decide(SyncState::Staged, false, SyncPolicy::ManualMerge),
            SyncDecision::PushLocal
        );
        assert_eq!(
            decide(SyncState::Staged, true, SyncPolicy::ManualMerge),
            SyncDecision::PushLocal
        );
    }

    #[test]
    fn synced_with_dirty_remote_pulls() {
        assert_eq!(
            decide(SyncState::Synced, true, SyncPolicy::ManualMerge),
            SyncDecision::PullRemote
        );
        assert_eq!(
            decide(SyncState::Synced, false, SyncPolicy::ManualMerge),
            SyncDecision::Noop
        );
    }

    #[test]
    fn dirty_local_with_dirty_remote_respects_policy() {
        assert_eq!(
            decide(SyncState::DirtyLocal, true, SyncPolicy::LocalWins),
            SyncDecision::PushLocal
        );
        assert_eq!(
            decide(SyncState::DirtyLocal, true, SyncPolicy::RemoteWins),
            SyncDecision::PullRemote
        );
        assert_eq!(
            decide(SyncState::DirtyLocal, true, SyncPolicy::ManualMerge),
            SyncDecision::RequireManualMerge
        );
    }

    #[test]
    fn dirty_local_without_dirty_remote_always_pushes() {
        assert_eq!(
            decide(SyncState::DirtyLocal, false, SyncPolicy::ManualMerge),
            SyncDecision::PushLocal
        );
    }

    #[test]
    fn conflict_respects_policy() {
        assert_eq!(
            decide(SyncState::Conflict, false, SyncPolicy::LocalWins),
            SyncDecision::PushLocal
        );
        assert_eq!(
            decide(SyncState::Conflict, false, SyncPolicy::RemoteWins),
            SyncDecision::PullRemote
        );
        assert_eq!(
            decide(SyncState::Conflict, false, SyncPolicy::ManualMerge),
            SyncDecision::RequireManualMerge
        );
    }
}
