//! domain-sync — pure reconciliation rules over [`SyncState`]. No I/O.
//!
//! Decoupled from [`TaskStatus`]: `decide` only inspects sync state. The
//! caller is responsible for filtering out tasks whose status (archived,
//! blocked, etc.) makes them ineligible for sync.

use domain_core::{OutboxEntryId, TaskId, Timestamp};
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

// ---------- Outbox (RFC 0001 §3 D2) ----------------------------------------
//
// Mirror tasks send writes through an outbox: lifecycle / edit commands on a
// non-LocalOnly task enqueue an `OutboxEntry` that the daemon's drainer
// applies against the remote. Types only — the drainer itself lands in
// Stage 6. Until then nothing reads or writes these.

/// One outbound mutation queued for a mirror task. Variants cover both the
/// REST patch path (`UpdateRemote`) and every GraphQL mutation the
/// `RemoteProjectProvider` port exposes — same enqueue / drain / retry
/// machinery handles both axes (per RFC 0001 §3 D2).
///
/// `#[serde(tag = "kind")]` keeps the on-disk `mutation_kind` discriminator
/// (the SQLite indexable column) in lockstep with the serialized payload,
/// so the drainer can route a row to the right adapter without a separate
/// kind field falling out of sync with the payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutboxMutation {
    /// REST `PATCH /repos/{o}/{r}/issues/{number}`. Carries the canonical
    /// repo so the drainer doesn't have to re-resolve the binding.
    UpdateRemote {
        canonical_repo: String,
        remote_id: String,
        title: Option<String>,
        body: Option<String>,
        closed: Option<bool>,
    },
    /// GraphQL `addProjectV2ItemById` — attach an existing issue to a project.
    AddItem {
        project_node_id: String,
        issue_node_id: String,
    },
    /// GraphQL `addProjectV2DraftIssue` — create a draft directly in a
    /// project. Used when promoting an orphan task (no `repo_id`).
    CreateDraftIssue {
        project_node_id: String,
        title: String,
        body: String,
    },
    /// GraphQL `updateProjectV2DraftIssue` — drafts have no REST counterpart,
    /// so this is the only mutation path for an orphan task's content.
    UpdateDraftIssue {
        item_node_id: String,
        title: Option<String>,
        body: Option<String>,
    },
    /// GraphQL `convertProjectV2DraftIssueItemToIssue` — fires when an
    /// orphan task gets `--repo` attached and graduates from draft to issue.
    /// The project item retains its node ID; only the content union shifts.
    ConvertDraftToIssue {
        item_node_id: String,
        repo_node_id: String,
    },
    /// GraphQL `updateProjectV2ItemFieldValue` against the single-select
    /// Status field. Works on both draft items and issue-backed items.
    SetProjectStatus {
        project_node_id: String,
        item_node_id: String,
        status_field_id: String,
        option_id: String,
    },
}

impl OutboxMutation {
    /// Discriminator stored in the `outbox_entries.mutation_kind` column
    /// alongside the serialized payload. Kept in lockstep with the serde
    /// `#[serde(tag = "kind")]` tags so reads decode cleanly.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::UpdateRemote { .. } => "update_remote",
            Self::AddItem { .. } => "add_item",
            Self::CreateDraftIssue { .. } => "create_draft_issue",
            Self::UpdateDraftIssue { .. } => "update_draft_issue",
            Self::ConvertDraftToIssue { .. } => "convert_draft_to_issue",
            Self::SetProjectStatus { .. } => "set_project_status",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxStatus {
    Pending,
    Inflight,
    Succeeded,
    Failed,
}

/// One row of the outbox. Append-only from the caller's perspective; the
/// drainer flips `status` and bumps `attempts` / `last_error` as it works
/// each entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxEntry {
    pub id: OutboxEntryId,
    pub task_id: TaskId,
    pub mutation: OutboxMutation,
    pub status: OutboxStatus,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub enqueued_at: Timestamp,
    pub updated_at: Timestamp,
}

impl OutboxEntry {
    /// Mint a fresh `Pending` entry. `id` is a new UUID; `attempts` starts
    /// at zero. Callers don't choose timestamps — the entry's clock starts
    /// at enqueue time, not at the moment the underlying user action ran.
    pub fn new(task_id: TaskId, mutation: OutboxMutation) -> Self {
        let now = Timestamp::now();
        Self {
            id: OutboxEntryId::new(),
            task_id,
            mutation,
            status: OutboxStatus::Pending,
            attempts: 0,
            last_error: None,
            enqueued_at: now,
            updated_at: now,
        }
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

    #[test]
    fn outbox_mutation_kind_matches_serde_tag() {
        // Lock the on-disk discriminator to the serde tag — a serde rename
        // here without a `kind()` arm update would silently desync the
        // SQLite column from the payload.
        let m = OutboxMutation::AddItem {
            project_node_id: "PVT_x".into(),
            issue_node_id: "I_y".into(),
        };
        assert_eq!(m.kind(), "add_item");
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["kind"], "add_item");
    }

    #[test]
    fn outbox_entry_new_starts_pending_with_zero_attempts() {
        let m = OutboxMutation::SetProjectStatus {
            project_node_id: "PVT_x".into(),
            item_node_id: "PVTI_y".into(),
            status_field_id: "PVTSSF_z".into(),
            option_id: "abc12345".into(),
        };
        let entry = OutboxEntry::new(TaskId::new(), m);
        assert_eq!(entry.status, OutboxStatus::Pending);
        assert_eq!(entry.attempts, 0);
        assert!(entry.last_error.is_none());
    }
}
