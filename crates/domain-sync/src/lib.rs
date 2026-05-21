//! domain-sync — pure reconciliation rules over task states. No I/O.

use domain_task::TaskState;
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

/// Decide what to do for a single task given local state, whether the remote
/// snapshot is known-dirty, and the configured policy.
pub fn decide(local: TaskState, remote_dirty: bool, policy: SyncPolicy) -> SyncDecision {
    match local {
        TaskState::Draft | TaskState::Archived | TaskState::Blocked => SyncDecision::Noop,
        TaskState::Staged | TaskState::DirtyLocal => SyncDecision::PushLocal,
        TaskState::Pushed | TaskState::Synced if remote_dirty => SyncDecision::PullRemote,
        TaskState::Pushed | TaskState::Synced => SyncDecision::Noop,
        TaskState::DirtyRemote => SyncDecision::PullRemote,
        TaskState::Conflict => match policy {
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
    fn draft_is_never_synced() {
        assert_eq!(
            decide(TaskState::Draft, true, SyncPolicy::ManualMerge),
            SyncDecision::Noop
        );
    }

    #[test]
    fn staged_pushes_regardless_of_remote() {
        assert_eq!(
            decide(TaskState::Staged, false, SyncPolicy::ManualMerge),
            SyncDecision::PushLocal
        );
        assert_eq!(
            decide(TaskState::Staged, true, SyncPolicy::ManualMerge),
            SyncDecision::PushLocal
        );
    }

    #[test]
    fn synced_with_dirty_remote_pulls() {
        assert_eq!(
            decide(TaskState::Synced, true, SyncPolicy::ManualMerge),
            SyncDecision::PullRemote
        );
        assert_eq!(
            decide(TaskState::Synced, false, SyncPolicy::ManualMerge),
            SyncDecision::Noop
        );
    }

    #[test]
    fn conflict_respects_policy() {
        assert_eq!(
            decide(TaskState::Conflict, false, SyncPolicy::LocalWins),
            SyncDecision::PushLocal
        );
        assert_eq!(
            decide(TaskState::Conflict, false, SyncPolicy::RemoteWins),
            SyncDecision::PullRemote
        );
        assert_eq!(
            decide(TaskState::Conflict, false, SyncPolicy::ManualMerge),
            SyncDecision::RequireManualMerge
        );
    }
}
