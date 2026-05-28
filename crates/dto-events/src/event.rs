//! The `DomainEvent` dispatch enum and its `EventEnvelope` wrapper.

use crate::payload::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DomainEvent {
    WorkspaceCreated(WorkspaceCreated),
    WorkspaceArchived(WorkspaceArchived),
    RepoAttached(RepoAttached),
    RepoDetached(RepoDetached),
    WorktreeRegistered(WorktreeRegistered),
    WorktreeMissing(WorktreeMissing),
    WorktreePruned(WorktreePruned),
    TaskCreated(TaskCreated),
    TaskStaged(TaskStaged),
    TaskPromoted(TaskPromoted),
    TaskSynced(TaskSynced),
    TaskDirtyLocal(TaskDirtyLocal),
    TaskDirtyRemote(TaskDirtyRemote),
    TaskConflicted(TaskConflicted),
    TaskBlocked(TaskBlocked),
    TaskArchived(TaskArchived),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub at: DateTime<Utc>,
    pub workspace_id: Option<String>,
    pub event: DomainEvent,
}
