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
    /// of the repo the issue lives in — the task's FILING repo (RFC 0002), which
    /// may differ from its logical repo for a cross-filed task — so the drainer
    /// doesn't have to re-resolve the binding.
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
    /// REST `POST /repos/{o}/{r}/issues/{parent}/sub_issues` — link an existing
    /// issue as a sub-issue of another (the GitHub-native projection of a
    /// `parent_of` / `child_of` relation). `parent_*` addresses the URL; GitHub
    /// wants the child's integer **database id** in the `sub_issue_id` body
    /// field (NOT its `#number`), so the drainer resolves `child_*` → db id at
    /// apply time, keeping enqueue offline. The db id is global, so a cross-repo
    /// child is representable here even though the import side skips them.
    AddSubIssue {
        parent_canonical: String,
        parent_remote_id: String,
        child_canonical: String,
        child_remote_id: String,
    },
    /// REST `DELETE /repos/{o}/{r}/issues/{parent}/sub_issue` (body
    /// `sub_issue_id`) — unlink a sub-issue. Same addressing / db-id resolution
    /// as [`OutboxMutation::AddSubIssue`].
    RemoveSubIssue {
        parent_canonical: String,
        parent_remote_id: String,
        child_canonical: String,
        child_remote_id: String,
    },
    /// REST `POST /repos/{o}/{r}/issues/{blocked}/dependencies/blocked_by` (body
    /// `issue_id` = the blocker's integer **database id**) — record an issue
    /// dependency (GitHub issue dependencies, GA 2025-08-21). The native
    /// projection of a `blocked_by` / `blocks` relation. `blocked_*` addresses
    /// the URL; the drainer resolves `blocker_*` → db id at apply time. Only the
    /// `blocked_by` side is written — `blocking` is GitHub's inverse read.
    AddBlockedBy {
        blocked_canonical: String,
        blocked_remote_id: String,
        blocker_canonical: String,
        blocker_remote_id: String,
    },
    /// REST `DELETE .../issues/{blocked}/dependencies/blocked_by/{issue_id}` —
    /// drop a dependency. The blocker's db id rides in the URL path, so the
    /// drainer resolves `blocker_*` → db id at apply time.
    RemoveBlockedBy {
        blocked_canonical: String,
        blocked_remote_id: String,
        blocker_canonical: String,
        blocker_remote_id: String,
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
            Self::AddSubIssue { .. } => "add_sub_issue",
            Self::RemoveSubIssue { .. } => "remove_sub_issue",
            Self::AddBlockedBy { .. } => "add_blocked_by",
            Self::RemoveBlockedBy { .. } => "remove_blocked_by",
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
    fn relation_mutation_kinds_match_serde_tags() {
        // Same lockstep guard as above, for the relation-sync variants: a
        // serde rename without a `kind()` arm update would desync the SQLite
        // discriminator from the payload.
        let cases: [(OutboxMutation, &str); 4] = [
            (
                OutboxMutation::AddSubIssue {
                    parent_canonical: "github.com/o/r".into(),
                    parent_remote_id: "1".into(),
                    child_canonical: "github.com/o/r".into(),
                    child_remote_id: "2".into(),
                },
                "add_sub_issue",
            ),
            (
                OutboxMutation::RemoveSubIssue {
                    parent_canonical: "github.com/o/r".into(),
                    parent_remote_id: "1".into(),
                    child_canonical: "github.com/o/r".into(),
                    child_remote_id: "2".into(),
                },
                "remove_sub_issue",
            ),
            (
                OutboxMutation::AddBlockedBy {
                    blocked_canonical: "github.com/o/r".into(),
                    blocked_remote_id: "1".into(),
                    blocker_canonical: "github.com/o/r".into(),
                    blocker_remote_id: "2".into(),
                },
                "add_blocked_by",
            ),
            (
                OutboxMutation::RemoveBlockedBy {
                    blocked_canonical: "github.com/o/r".into(),
                    blocked_remote_id: "1".into(),
                    blocker_canonical: "github.com/o/r".into(),
                    blocker_remote_id: "2".into(),
                },
                "remove_blocked_by",
            ),
        ];
        for (m, tag) in cases {
            assert_eq!(m.kind(), tag);
            let json = serde_json::to_value(&m).unwrap();
            assert_eq!(json["kind"], tag);
        }
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
