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
    /// Parent GitHub Projects v2 board node ID (`PVT_…`) when the workspace
    /// is linked to one. Omitted from JSON in the projectless case to keep
    /// the existing local-only shape unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateWorkspaceCmd {
    pub name: String,
    pub description: Option<String>,
    pub local_only: bool,
    /// Optional project to attach the new workspace to. Accepts a project
    /// node ID (`PVT_…`) or `owner/number`; resolution happens in
    /// `WorkspaceService::create` against the local `ProjectRepository`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_spec: Option<String>,
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
    pub name: String,
    pub aliases: Vec<String>,
    /// Globally-unique short handle used both as the human-typeable
    /// piece of friendly task IDs (`prefix-hash`) and as a stand-alone
    /// repo locator anywhere a binding ID is taken.
    pub prefix: String,
    pub worktrees: Vec<WorktreeLinkDto>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindRepoMatchDto {
    pub binding: RepoBindingDto,
    pub workspace_id: String,
    /// Which field matched: "name" | "alias" | "canonical_url" | "name_substring".
    pub matched_by: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindRepoResponseDto {
    pub query: String,
    pub matches: Vec<FindRepoMatchDto>,
    pub ambiguous: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachRepoCmd {
    pub workspace_id: String,
    pub remote_url: String,
    pub canonical_url: String,
    pub tracked_branch: Option<String>,
    /// Optional checkout path to register as a worktree on the binding.
    /// The CLI is responsible for verifying that the path's git origin
    /// canonicalises to `canonical_url`; the service trusts what it's
    /// handed and just records the link.
    pub link_path: Option<String>,
    pub link_branch: Option<String>,
    /// Explicit prefix override. When `None`, the service derives one
    /// from the repo name via [`domain_repo::derive_prefix`] and breaks
    /// collisions with a numeric suffix. When `Some`, the supplied
    /// value is validated against `^[a-z][a-z0-9]{1,19}$` and used
    /// verbatim — collisions surface as a `Conflict` error so the
    /// user is forced to pick a different prefix (rather than getting
    /// `myprefix1` silently).
    pub prefix: Option<String>,
}

/// Returned by `attach`: carries the resulting binding plus whether the
/// call merged into an existing one (same `canonical_url`) and which
/// path, if any, was newly linked. Lets agents distinguish "I created"
/// from "I joined" without comparing IDs out-of-band.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoAttachOutcomeDto {
    pub binding: RepoBindingDto,
    pub merged: bool,
    pub worktree_added: Option<String>,
}

/// One workspace-binding pair: this canonical URL is bound under `binding`
/// inside `workspace`. A repo can be a member of multiple workspaces, so
/// callers receive a `Vec<RepoMembershipDto>`. Used by `repo locate` and
/// `agents docs` to report cross-workspace membership.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMembershipDto {
    pub workspace: WorkspaceDto,
    pub binding: RepoBindingDto,
}

/// Full result of a `repo locate` lookup. `canonical_url` is `None` when
/// the queried path isn't a git repo with an origin remote; `matches` is
/// empty when no binding references the discovered remote.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocateResponseDto {
    pub query_path: String,
    pub canonical_url: Option<String>,
    pub matches: Vec<RepoMembershipDto>,
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
    /// Free-text caveat the CLI surfaces alongside a successful sync verb,
    /// when the operation completed but the user should know about an
    /// anomaly (e.g. linking to a URL whose live issue has been transferred
    /// elsewhere). `None` on the happy path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

// ---------- Project (RFC 0001) --------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusOptionDto {
    pub option_id: String,
    pub name: String,
    pub ordinal: u32,
    /// The local `TaskStatus` this option is the default for, if any.
    /// Mirrored from the project's `status_mappings` collection on
    /// serialization so the CLI can show "Backlog → Open" in one view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_for: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusMappingDto {
    pub status: String,
    pub option_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectDto {
    /// `PVT_…` — the GitHub node ID. No separate local UUID.
    pub id: String,
    pub owner_login: String,
    pub number: u64,
    pub title: String,
    pub status_field_id: String,
    pub status_options: Vec<StatusOptionDto>,
    pub status_mappings: Vec<StatusMappingDto>,
    pub archived: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Hand-entered project schema for `rl project link` in Stage 4. Stage 5
/// replaces these flags with a GraphQL fetch — the shape of the payload
/// is the same either way, so the service signature carries over.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkProjectCmd {
    pub node_id: String,
    pub owner_login: String,
    pub number: u64,
    pub title: String,
    pub status_field_id: String,
    pub status_options: Vec<StatusOptionDto>,
    /// Initial mappings the caller wants to seed (e.g. auto-derived from
    /// option-name match). Empty = no defaults set; user configures via
    /// `rl project map`.
    pub initial_mappings: Vec<StatusMappingDto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapStatusCmd {
    /// Project node id (`PVT_…`) or `owner/number` spec.
    pub project_spec: String,
    /// Local `TaskStatus` value as the snake-case string.
    pub status: String,
    pub option_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetWorkspaceProjectCmd {
    pub workspace_id: String,
    /// `None` means detach; `Some(spec)` accepts node id or `owner/number`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_spec: Option<String>,
}
