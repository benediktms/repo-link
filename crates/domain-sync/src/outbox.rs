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
    /// `repo_node_id` is a legacy field name: new entries carry the filing
    /// repo canonical URL, which the adapter resolves before the mutation.
    /// Older persisted entries may already contain a repository node ID.
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
    /// GraphQL `updateProjectV2ItemFieldValue` against the single-select
    /// Priority field (RFC 0006 D4). Sibling of [`Self::SetProjectStatus`] —
    /// same mutation, different field — kept as its OWN variant rather than
    /// overloading `SetProjectStatus` so the two projections stay
    /// independently addressable (a distinct outbox kind, a distinct drainer
    /// arm with no read-back `Conflict`, see the drainer doc). Priority rides
    /// the project-ITEM rail, never the issue PATCH path — it must stay out of
    /// `MIRRORED_FIELDS` / `MirrorPatch`.
    SetProjectPriority {
        project_node_id: String,
        item_node_id: String,
        priority_field_id: String,
        option_id: String,
    },
    /// GraphQL `updateProjectV2ItemFieldValue` (set) or
    /// `clearProjectV2ItemFieldValue` (clear) against a board's CUSTOM
    /// "Type"/"Types" single-select field (RFC 0006 #238). The dogfoodable
    /// alternative to [`Self::SetIssueType`] for user-owned boards, which have
    /// no native issue types: the local `IssueType` is projected onto the board
    /// option whose name matches (case-insensitively). `option_id` is `None` to
    /// clear the field value (via `clearProjectV2ItemFieldValue`), `Some(id)` to
    /// set it.
    ///
    /// Sibling of [`Self::SetProjectPriority`] — same project-ITEM rail, its own
    /// variant + drainer arm with no read-back `Conflict`. It carries the
    /// `local_issue_type` compare-and-clear fields of [`Self::SetIssueType`]
    /// because the two rails share the task's single durable projection intent
    /// (`Task::issue_type_pending`): whichever rail applies clears it on success
    /// (RFC 0006 #238 decision 2 — never both rails for one task). Exactly one
    /// of `SetProjectType` / `SetIssueType` is ever enqueued for a given edit,
    /// chosen by `resolve_type_projection`.
    SetProjectType {
        project_node_id: String,
        item_node_id: String,
        type_field_id: String,
        option_id: Option<String>,
        /// Canonical local value this mutation represents; compare-and-clears
        /// the durable projection intent on success (see [`Self::SetIssueType`]).
        #[serde(default)]
        local_issue_type: Option<String>,
        /// Distinguishes a newly-authored clear from a legacy serialized entry.
        #[serde(default)]
        local_issue_type_recorded: bool,
    },
    /// GraphQL `updateIssue(input: { id, issueTypeId })` — set (or clear) the
    /// issue's native "Type" field (RFC 0006 §0 A1 / #228). `issue_type_id`
    /// is `None` to clear, `Some(id)` to set (an `OrgIssueType::issue_type_id`
    /// resolved against the org registry at plan time).
    ///
    /// Deliberately OFF the issue mirror axis — NOT a `MIRRORED_FIELDS`
    /// member and never folded into `MirrorPatch` / `UpdateRemote`: octocrab's
    /// REST issue builder has no `issue_type` slot, so type can only be
    /// projected via this dedicated GraphQL mutation, exactly as
    /// `SetProjectPriority` sits off the project-item mirror. The outbox
    /// concrete outbox entry is the network retry unit; unresolved intent is
    /// retained separately on the task until a node id and registry mapping
    /// exist. There is no mirror baseline to re-confirm against
    /// (`Task::set_issue_type` does not call `reconcile_dirty_against_baseline`),
    /// so the drainer never flips `sync_state` or Conflicts on a read-back
    /// (there IS no read-back — see `GraphqlClient::set_issue_type`).
    SetIssueType {
        issue_node_id: String,
        issue_type_id: Option<String>,
        /// Canonical local value this mutation represents. Used only to
        /// compare-and-clear the task's durable projection intent after the
        /// remote confirms success.
        #[serde(default)]
        local_issue_type: Option<String>,
        /// Distinguishes a newly-authored clear (`local_issue_type = None`)
        /// from a legacy serialized entry that predates the field.
        #[serde(default)]
        local_issue_type_recorded: bool,
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
            Self::SetProjectPriority { .. } => "set_project_priority",
            Self::SetProjectType { .. } => "set_project_type",
            Self::SetIssueType { .. } => "set_issue_type",
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
    fn set_project_priority_kind_matches_serde_tag() {
        // Same lockstep guard as `outbox_mutation_kind_matches_serde_tag`, for
        // the new priority-projection variant (RFC 0006 D4 / #225): a serde
        // rename here without a `kind()` arm update would silently desync the
        // SQLite discriminator column from the payload.
        let m = OutboxMutation::SetProjectPriority {
            project_node_id: "PVT_x".into(),
            item_node_id: "PVTI_y".into(),
            priority_field_id: "PVTSSF_prio".into(),
            option_id: "opt_p0".into(),
        };
        assert_eq!(m.kind(), "set_project_priority");
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["kind"], "set_project_priority");
    }

    #[test]
    fn set_project_type_kind_matches_serde_tag() {
        // Lockstep guard for the custom Type-projection variant (RFC 0006
        // #238). Cover both set and clear (`option_id: None`) shapes — both
        // serialize under the same `kind`.
        let set = OutboxMutation::SetProjectType {
            project_node_id: "PVT_x".into(),
            item_node_id: "PVTI_y".into(),
            type_field_id: "PVTSSF_type".into(),
            option_id: Some("opt_bug".into()),
            local_issue_type: Some("bug".into()),
            local_issue_type_recorded: true,
        };
        assert_eq!(set.kind(), "set_project_type");
        assert_eq!(
            serde_json::to_value(&set).unwrap()["kind"],
            "set_project_type"
        );

        let clear = OutboxMutation::SetProjectType {
            project_node_id: "PVT_x".into(),
            item_node_id: "PVTI_y".into(),
            type_field_id: "PVTSSF_type".into(),
            option_id: None,
            local_issue_type: None,
            local_issue_type_recorded: true,
        };
        assert_eq!(clear.kind(), "set_project_type");
        assert_eq!(
            serde_json::to_value(&clear).unwrap()["kind"],
            "set_project_type"
        );
    }

    #[test]
    fn set_issue_type_kind_matches_serde_tag() {
        // Same lockstep guard as `set_project_priority_kind_matches_serde_tag`,
        // for the new type-projection variant (RFC 0006 §0 A1 / #228): a serde
        // rename here without a `kind()` arm update would silently desync the
        // SQLite discriminator column from the payload. Cover both the "set"
        // and "clear" (`issue_type_id: None`) shapes — both must serialize
        // under the same `kind`.
        let set = OutboxMutation::SetIssueType {
            issue_node_id: "I_x".into(),
            issue_type_id: Some("IT_bug".into()),
            local_issue_type: Some("bug".into()),
            local_issue_type_recorded: true,
        };
        assert_eq!(set.kind(), "set_issue_type");
        let json = serde_json::to_value(&set).unwrap();
        assert_eq!(json["kind"], "set_issue_type");

        let clear = OutboxMutation::SetIssueType {
            issue_node_id: "I_x".into(),
            issue_type_id: None,
            local_issue_type: None,
            local_issue_type_recorded: true,
        };
        assert_eq!(clear.kind(), "set_issue_type");
        let json = serde_json::to_value(&clear).unwrap();
        assert_eq!(json["kind"], "set_issue_type");
    }

    #[test]
    fn legacy_set_issue_type_payload_does_not_claim_a_local_value() {
        let mutation: OutboxMutation = serde_json::from_value(serde_json::json!({
            "kind": "set_issue_type",
            "issue_node_id": "I_x",
            "issue_type_id": "IT_bug"
        }))
        .unwrap();
        assert!(matches!(
            mutation,
            OutboxMutation::SetIssueType {
                local_issue_type: None,
                local_issue_type_recorded: false,
                ..
            }
        ));
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
