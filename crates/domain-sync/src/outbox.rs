//! Outbox (RFC 0001 §3 D2).
//!
//! Mirror tasks send writes through an outbox: lifecycle / edit commands on a
//! non-LocalOnly task enqueue an `OutboxEntry` that the daemon's drainer
//! applies against the remote. Types only — the drainer itself lands in
//! Stage 6. Until then nothing reads or writes these.

use domain_core::{OutboxEntryId, TaskId, Timestamp};
use serde::{Deserialize, Serialize};

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
    /// REST `PATCH /repos/{o}/{r}/issues/{number}`. Carries the canonical URL
    /// of the repo the issue lives in — the task's filing repo, which today is
    /// always its logical repo (until RFC 0002) — so the drainer doesn't have
    /// to re-resolve the binding.
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
    /// project. Used when promoting an orphan task (no logical `repo_id`, so
    /// no repo to file an issue in): the draft lives only on the board until a
    /// repo is attached and it converts to a real issue.
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
    /// `repo_node_id` is the repo the issue is filed in — today the task's
    /// logical repo, until RFC 0002 lets a separate filing repo decide this.
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
    /// Earliest instant the drainer may re-claim this entry. `None` means
    /// "eligible immediately" — the state of every freshly-enqueued entry.
    /// After a recoverable failure under the attempt cap, the drainer sets
    /// this to `now + backoff(attempts)` and flips `status` back to
    /// `Pending` (RFC 0001 §10.2). The claim query honours
    /// `next_attempt_at IS NULL OR next_attempt_at <= now`.
    #[serde(default)]
    pub next_attempt_at: Option<Timestamp>,
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
            next_attempt_at: None,
            enqueued_at: now,
            updated_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
