//! domain-sync — pure reconciliation rules over [`SyncState`]. No I/O.
//!
//! Decoupled from [`TaskStatus`]: `decide` only inspects sync state. The
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    LocalEditedRemoteEdited,
    RemoteDeletedLocalEdited,
    LocalDeletedRemoteEdited,
    AssigneeMismatch,
    StatusMismatch,
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
