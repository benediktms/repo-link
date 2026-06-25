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
    /// Lifecycle open/closed bit (RFC 0004 D1): `true` for `Lifecycle::Open` /
    /// `Reopened`, `false` for `Completed` / `NotPlanned`. The old 5-state
    /// `status: String` is gone; "blocked" is no longer a state (it's derived
    /// from relations).
    pub is_open: bool,
    /// The GitHub-style close reason, decomposed from the `Lifecycle` so JSON
    /// consumers keep both axes: `Some("completed")` for `Completed`,
    /// `Some("not_planned")` for `NotPlanned`, `Some("reopened")` for
    /// `Reopened`, and `None` for `Open`.
    pub state_reason: Option<String>,
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
    /// The task's **logical repo** binding (a repo UUID): where the code lives
    /// and the source of the friendly-ID prefix. Today the issue is also filed
    /// here on promote (logical == filing repo until RFC 0002). `None` creates
    /// an orphan task (a project-board draft).
    pub repo_id: Option<String>,
    pub title: String,
    pub body: Option<String>,
    pub priority: Option<String>,
    /// RFC 0002 D2 step-1 per-task filing-repo override (a repo UUID). Takes
    /// highest precedence in the D2 chain (beats workspace default and logical
    /// repo). Distinct from `repo_id` (the logical axis) and NEVER named
    /// `filing_repo_id` (D5 guard, #119). Carried here for wiring completeness;
    /// `TaskService::create` does NOT consume it today because `task create`
    /// only mints a `LocalOnly` draft and never promotes — the override has no
    /// filing transition to feed until `sync promote` consumes it. A
    /// non-promoting create that supplies this field is rejected at the CLI
    /// boundary with a deferral error pointing at `rl sync promote` (RFC 0002
    /// §4, #122 brief preference (a)).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filing_repo_override: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateTaskCmd {
    pub task_id: String,
    pub title: Option<String>,
    pub body: Option<String>,
    pub priority: Option<String>,
    pub assignees: Option<Vec<String>>,
    /// Reassign the task's **logical repo** binding (a repo UUID): where the
    /// code/worktrees live and the prefix source — today also the filing repo
    /// on promote (until RFC 0002). `None` leaves the current logical repo
    /// untouched. Only valid while the task is not yet remote-backed — the
    /// service rejects reassigning a synced task. There is no way to *clear*
    /// the repo via update (matches the assignees gap).
    pub repo_id: Option<String>,
}

/// Materialise a remote issue as a local mirror task (`sync import`). The
/// CLI fetches the issue + resolves the binding, then hands the application
/// layer everything needed to construct a `Synced` task with a `Pull`
/// baseline. `repo_id` is the resolved **logical repo** binding UUID — the
/// repo the imported issue lives in, which is also that task's logical repo
/// (logical == filing repo until RFC 0002). `closed` maps to the initial
/// lifecycle status (open→Open, closed→Done).
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
    /// Lifecycle filter (RFC 0004 D1): `"open"` / `"closed"`, or `None` for all.
    pub status: Option<String>,
    pub sync_state: Option<String>,
}
