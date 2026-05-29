use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
pub struct TaskCommentDto {
    /// GitHub comment id; `None` for a pending local comment not yet pushed.
    pub remote_id: Option<String>,
    pub author: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
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
    /// Mirrored issue comments (oldest first). Populated for `task show`;
    /// empty in list views to avoid a per-row fetch.
    pub comments: Vec<TaskCommentDto>,
    /// Cached GitHub Projects v2 board status as a display name (e.g.
    /// `"In progress"`), resolved from the task's cached
    /// `project_status_option_id` via its workspace's project (RFC 0001
    /// Stage 8, closes #39). `None` when the task is projectless, hasn't been
    /// polled yet, or its cached option id is no longer owned by the project.
    /// Read from the local cache only — `rl task show` does ZERO network I/O.
    /// Additive; defaults to null for older consumers.
    #[serde(default)]
    pub project_status: Option<String>,
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
    /// Reassign the owning repo binding (a repo UUID). `None` leaves the
    /// current repo untouched. Only valid while the task is not yet
    /// remote-backed — the service rejects reassigning a synced task.
    /// There is no way to *clear* the repo via update (matches the
    /// assignees gap).
    pub repo_id: Option<String>,
}

/// Materialise a remote issue as a local mirror task (`sync import`). The
/// CLI fetches the issue + resolves the binding, then hands the application
/// layer everything needed to construct a `Synced` task with a `Pull`
/// baseline. `repo_id` is the resolved binding UUID; `closed` maps to the
/// initial lifecycle status (open→Open, closed→Done).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportMirrorCmd {
    pub workspace_id: String,
    pub repo_id: Option<String>,
    pub provider: String,
    pub remote_id: String,
    pub title: String,
    pub body: String,
    pub assignees: Vec<String>,
    pub closed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddTaskRelationCmd {
    pub task_id: String,
    pub kind: String,
    pub other: String,
}

/// Remove a single relation edge. The service strips the reciprocal edge
/// from the other task too. For clearing *all* relations, the service
/// exposes a separate `clear_relations` entry point.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveTaskRelationCmd {
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
