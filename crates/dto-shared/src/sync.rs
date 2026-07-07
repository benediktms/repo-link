use serde::{Deserialize, Serialize};

use crate::RemoteRefDto;

// ---------- Sync ----------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromoteTaskCmd {
    pub task_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushTaskCmd {
    pub task_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullTaskCmd {
    pub task_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncSummaryDto {
    pub task_id: String,
    pub previous_state: String,
    pub new_state: String,
    pub decision: String,
    pub remote: Option<RemoteRefDto>,
    /// Free-text caveat the CLI surfaces alongside a successful sync verb,
    /// when the operation completed but the user should know about an
    /// anomaly (e.g. linking to a URL whose live issue has been transferred
    /// elsewhere). `None` on the happy path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Structured notices produced by a sync verb — currently only `pull`,
    /// which emits one per inbound relation edge it reconciled from the remote
    /// (so a change to the local graph is never silent) and one per remote
    /// related-issue that has no local task to link. Empty on the happy path;
    /// the other verbs never populate it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<SyncNoticeDto>,
}

/// A user-facing notice attached to a sync result. Polymorphic and
/// internally-tagged (`"kind"`) so new notice kinds can be added without
/// breaking the wire shape — modeled on `dto-events::DomainEvent`. Structured
/// only: the human-readable line is formatted by the CLI renderer, not stored
/// here, so there is one source of truth for the prose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncNoticeDto {
    /// A relation edge on the subject task was added or removed locally to
    /// match the remote (e.g. a re-parent, or a dependency the remote dropped).
    RelationReconciled(RelationReconciledNotice),
    /// The remote reports a related issue (parent/child/blocker) that has no
    /// local task, so the edge could not be replicated. Surfaces the remote
    /// issue's identity so the user can import it and re-link.
    RelationTargetUntracked(RelationTargetUntrackedNotice),
}

/// Whether an edge was added or removed during reconcile. An enum (not a
/// string) so producer and CLI renderer are compiler-checked to agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationChange {
    Added,
    Removed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationReconciledNotice {
    /// The subject task whose edge changed.
    pub task_id: String,
    /// The subject's edge kind, as snake_case `RelationKind` (e.g. `child_of`).
    /// Named `relation_kind` to avoid colliding with the enum's `kind` tag.
    pub relation_kind: String,
    pub change: RelationChange,
    /// The local task the edge points to (added) or was removed from.
    pub other_task_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationTargetUntrackedNotice {
    /// The subject task the untracked issue is related to.
    pub task_id: String,
    /// The subject's edge kind toward the untracked issue, snake_case
    /// `RelationKind` (e.g. `child_of` when the untracked issue is the parent).
    /// Named `relation_kind` to avoid colliding with the enum's `kind` tag.
    pub relation_kind: String,
    /// Canonical repo of the untracked remote issue, e.g. `github.com/o/r`.
    pub canonical_repo: String,
    /// The untracked remote issue's id (`#number`).
    pub remote_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_notice_serializes_with_snake_case_kind_tag() {
        let reconciled = SyncNoticeDto::RelationReconciled(RelationReconciledNotice {
            task_id: "t1".into(),
            relation_kind: "child_of".into(),
            change: RelationChange::Removed,
            other_task_id: "t2".into(),
        });
        let v = serde_json::to_value(&reconciled).unwrap();
        assert_eq!(v["kind"], "relation_reconciled");
        assert_eq!(v["relation_kind"], "child_of");
        assert_eq!(v["change"], "removed");
        assert_eq!(reconciled, serde_json::from_value(v).unwrap());

        let untracked = SyncNoticeDto::RelationTargetUntracked(RelationTargetUntrackedNotice {
            task_id: "t1".into(),
            relation_kind: "child_of".into(),
            canonical_repo: "github.com/o/r".into(),
            remote_id: "42".into(),
        });
        let v = serde_json::to_value(&untracked).unwrap();
        assert_eq!(v["kind"], "relation_target_untracked");
        assert_eq!(untracked, serde_json::from_value(v).unwrap());
    }

    #[test]
    fn summary_omits_empty_messages() {
        let dto = SyncSummaryDto {
            task_id: "t1".into(),
            previous_state: "synced".into(),
            new_state: "synced".into(),
            decision: "noop".into(),
            remote: None,
            note: None,
            messages: vec![],
        };
        let v = serde_json::to_value(&dto).unwrap();
        assert!(v.get("messages").is_none());
        assert!(v.get("note").is_none());
    }
}
