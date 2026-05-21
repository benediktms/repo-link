//! dto-events — domain event payloads for the audit log and future streams.

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

// ---------- Payload structs ----------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCreated {
    pub workspace_id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceArchived {
    pub workspace_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoAttached {
    pub repo_id: String,
    pub workspace_id: String,
    pub remote_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoDetached {
    pub repo_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRegistered {
    pub repo_id: String,
    pub path: String,
    pub branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeMissing {
    pub repo_id: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreePruned {
    pub repo_id: String,
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCreated {
    pub task_id: String,
    pub workspace_id: String,
    pub repo_id: Option<String>,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStaged {
    pub task_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPromoted {
    pub task_id: String,
    pub provider: String,
    pub remote_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSynced {
    pub task_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDirtyLocal {
    pub task_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDirtyRemote {
    pub task_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskConflicted {
    pub task_id: String,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBlocked {
    pub task_id: String,
    pub blocked_by: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskArchived {
    pub task_id: String,
}
