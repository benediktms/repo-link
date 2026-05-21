//! dto-shared — command/query/response payloads crossing layer boundaries.
//!
//! IDs are strings here on purpose: DTOs cross JSON, SQL TEXT columns, and
//! external API responses, so they stay free of the typed `domain-core`
//! newtypes. The application layer converts at the boundary.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------- Workspace -----------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDto {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub status: String,
    pub local_only: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateWorkspaceCmd {
    pub name: String,
    pub description: Option<String>,
    pub local_only: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListWorkspacesQuery {
    pub include_archived: bool,
}

// ---------- Repo binding --------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeLinkDto {
    pub path: String,
    pub branch: Option<String>,
    pub status: String,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoBindingDto {
    pub id: String,
    pub workspace_id: String,
    pub remote_url: String,
    pub canonical_url: String,
    pub tracked_branch: Option<String>,
    pub worktrees: Vec<WorktreeLinkDto>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachRepoCmd {
    pub workspace_id: String,
    pub remote_url: String,
    pub canonical_url: String,
    pub tracked_branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkWorktreeCmd {
    pub repo_id: String,
    pub path: String,
    pub branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnlinkWorktreeCmd {
    pub repo_id: String,
    pub path: String,
}

// ---------- Task ----------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRefDto {
    pub provider: String,
    pub remote_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRelationDto {
    pub kind: String,
    pub other: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDto {
    pub id: String,
    pub workspace_id: String,
    pub repo_id: Option<String>,
    pub title: String,
    pub body: String,
    /// Lifecycle status: `open` / `in_progress` / `blocked` / `done` / `archived`.
    pub status: String,
    /// Sync state: `local_only` / `staged` / `synced` / `dirty_local` / `dirty_remote` / `conflict`.
    pub sync_state: String,
    pub priority: String,
    pub assignees: Vec<String>,
    pub remote: Option<RemoteRefDto>,
    pub relations: Vec<TaskRelationDto>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateTaskCmd {
    pub workspace_id: String,
    pub repo_id: Option<String>,
    pub title: String,
    pub body: Option<String>,
    pub priority: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateTaskCmd {
    pub task_id: String,
    pub title: Option<String>,
    pub body: Option<String>,
    pub priority: Option<String>,
    pub assignees: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddTaskRelationCmd {
    pub task_id: String,
    pub kind: String,
    pub other: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListTasksQuery {
    pub workspace_id: Option<String>,
    pub repo_id: Option<String>,
    pub status: Option<String>,
    pub sync_state: Option<String>,
    pub include_archived: bool,
}

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
}
