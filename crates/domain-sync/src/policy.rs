//! Pure reconciliation rules over [`SyncState`]. No I/O.
//!
//! Decoupled from `TaskStatus`: `decide` only inspects sync state. The
//! caller is responsible for filtering out tasks whose status (archived,
//! blocked, etc.) makes them ineligible for sync.

use domain_task::SyncState;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncDecision {
    Noop,
    PushLocal,
    PullRemote,
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
/// into the inbound mirror set (a local-vs-remote flip resolves remote-wins
/// through the generic `decide()` → `PullRemote` path). The old `StatusMismatch`
/// variant that modelled that case was removed; see RFC 0004 D1 + RFC 0003 §6
/// OQ5 for the reasoning so it is not re-derived.
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

/// Decide what to do for a single task given its sync state and whether the
/// remote snapshot is known-dirty.
///
/// **GitHub is authoritative once a task is remote-backed (#290).** A local
/// edit is a proposal against the last observed remote state, so when the two
/// sides have both moved the answer is `PullRemote` — the proposal is
/// discarded and the cache converges. There is no arbitration policy: the two
/// sides are not peers. The discarded proposal stays recoverable through the
/// `PrePull` snapshot the caller writes, and `rl sync push --force` remains
/// the deliberate local-wins override.
pub fn decide(sync: SyncState, remote_dirty: bool) -> SyncDecision {
    match sync {
        SyncState::LocalOnly => SyncDecision::Noop,
        SyncState::Staged => SyncDecision::PushLocal,
        SyncState::DirtyLocal if remote_dirty => SyncDecision::PullRemote,
        SyncState::DirtyLocal => SyncDecision::PushLocal,
        SyncState::Synced if remote_dirty => SyncDecision::PullRemote,
        SyncState::Synced => SyncDecision::Noop,
        SyncState::DirtyRemote | SyncState::Conflict => SyncDecision::PullRemote,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_only_is_never_synced() {
        assert_eq!(decide(SyncState::LocalOnly, true), SyncDecision::Noop);
    }

    #[test]
    fn staged_pushes_regardless_of_remote() {
        assert_eq!(decide(SyncState::Staged, false), SyncDecision::PushLocal);
        assert_eq!(decide(SyncState::Staged, true), SyncDecision::PushLocal);
    }

    #[test]
    fn synced_with_dirty_remote_pulls() {
        assert_eq!(decide(SyncState::Synced, true), SyncDecision::PullRemote);
        assert_eq!(decide(SyncState::Synced, false), SyncDecision::Noop);
    }

    /// The #290 rule: a local proposal that raced a newer remote change loses.
    #[test]
    fn dirty_local_with_dirty_remote_pulls() {
        assert_eq!(
            decide(SyncState::DirtyLocal, true),
            SyncDecision::PullRemote
        );
    }

    #[test]
    fn dirty_local_without_dirty_remote_always_pushes() {
        assert_eq!(
            decide(SyncState::DirtyLocal, false),
            SyncDecision::PushLocal
        );
    }

    /// A task already parked in `Conflict` converges on the next pull instead
    /// of waiting for a human — including when the remote has not moved since.
    #[test]
    fn conflict_pulls_remote() {
        assert_eq!(decide(SyncState::Conflict, false), SyncDecision::PullRemote);
        assert_eq!(decide(SyncState::Conflict, true), SyncDecision::PullRemote);
    }

    #[test]
    fn dirty_remote_pulls() {
        assert_eq!(
            decide(SyncState::DirtyRemote, false),
            SyncDecision::PullRemote
        );
    }
}
