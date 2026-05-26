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
    /// value is validated against `^[a-z][a-z0-9]{1,7}$` and used
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
