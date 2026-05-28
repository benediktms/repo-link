//! Value types hung off a Task: relations, remote refs, comments.

use domain_core::{TaskId, Timestamp};
use serde::{Deserialize, Serialize};

use crate::enums::RelationKind;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRelation {
    pub kind: RelationKind,
    pub other: TaskId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRef {
    pub provider: String,
    /// Provider-native numeric identifier (e.g. GitHub issue `number`). The
    /// historical primary key from the REST world; kept because every CLI
    /// surface still speaks numbers.
    pub remote_id: String,
    /// Provider-native opaque node ID (e.g. GitHub `I_kwHO…`). Required by
    /// GraphQL mutations such as `addProjectV2ItemById`. Stays `None` for
    /// rows that pre-date the column or for providers that have no
    /// equivalent — the field is purely additive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}

impl RemoteRef {
    /// Build a `RemoteRef` carrying just the provider+number pair. Defaults
    /// `node_id` to `None`, matching every pre-Stage-2 construction site.
    pub fn new(provider: impl Into<String>, remote_id: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            remote_id: remote_id.into(),
            node_id: None,
        }
    }
}

/// A comment mirrored from (or destined for) the remote issue. `remote_id`
/// is `None` for a comment authored locally that hasn't been pushed yet —
/// the outbound path (a follow-up) sets it once the remote create succeeds.
/// Comments are append-only and orthogonal to the snapshot/dirty machinery:
/// they're never part of [`TaskSnapshot`], so remote-side comment activity
/// doesn't perturb dirty detection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskComment {
    /// Storage surrogate id (`task_comments.id`). `Some` once persisted —
    /// needed so the outbound drain can replace a specific row by identity
    /// rather than by `remote_id=''` predicate (which would race-delete a
    /// pending comment added between push reading the task and the drain).
    pub local_id: Option<String>,
    pub remote_id: Option<String>,
    pub author: String,
    pub body: String,
    pub created_at: Timestamp,
}
