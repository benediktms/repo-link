//! Remote project provider port and its DTOs, plus the project repository.

use async_trait::async_trait;
use domain_core::{ProjectId, Timestamp, WorkspaceId};
use domain_project::Project;

use crate::error::PortResult;

// ---------- Project ports (RFC 0001 §3 D1 / §6) ----------------------------

#[derive(Clone, Debug)]
pub struct RemoteProjectSnapshot {
    /// `PVT_…` — also the value stored as `projects.id` locally (no separate
    /// UUID; projects are a 100% mirror of the remote entity).
    pub node_id: String,
    pub number: u64,
    pub title: String,
    pub owner_login: String,
    pub status_field_id: String,
    pub status_options: Vec<RemoteProjectStatusOption>,
}

#[derive(Clone, Debug)]
pub struct RemoteProjectStatusOption {
    pub option_id: String,
    pub name: String,
    pub ordinal: u32,
}

#[derive(Clone, Debug)]
pub struct RemoteProjectItem {
    pub item_node_id: String,
    /// `None` for draft items — drafts have no underlying issue.
    pub issue_node_id: Option<String>,
    /// `None` for drafts; populated for issue-backed items so the daemon
    /// can correlate a polled item with its local repo binding without a
    /// follow-up REST call.
    pub canonical_repo: Option<String>,
    pub number: Option<u64>,
    pub title: String,
    pub body: String,
    pub closed: bool,
    pub status_option_id: Option<String>,
    pub updated_at: Timestamp,
}

#[async_trait]
pub trait RemoteProjectProvider: Send + Sync {
    /// Resolve `owner/number` → project schema. Called once per `rl project
    /// link` to learn the project's Status field id and option catalog.
    async fn fetch_project(&self, owner: &str, number: u64) -> PortResult<RemoteProjectSnapshot>;

    /// Attach an existing issue to a project. Returns the new item's
    /// `PVTI_…` node ID. Idempotent: re-calling for the same content
    /// returns the existing item ID rather than creating a duplicate row.
    async fn add_item(&self, project_node_id: &str, issue_node_id: &str) -> PortResult<String>;

    /// Create a draft issue directly in the project. Returns the new item's
    /// node ID. Used when promoting an orphan task (no `repo_id`).
    async fn create_draft_issue(
        &self,
        project_node_id: &str,
        title: &str,
        body: &str,
    ) -> PortResult<String>;

    /// Update a draft issue's title and/or body. Drafts have no REST
    /// counterpart, so this is the only mutation path for an orphan task's
    /// content.
    async fn update_draft_issue(
        &self,
        item_node_id: &str,
        title: Option<&str>,
        body: Option<&str>,
    ) -> PortResult<()>;

    /// Convert a draft item to a real issue in `repo_node_id`. The item
    /// retains its node ID; only the content union shifts from
    /// `ProjectV2DraftIssue` to `Issue`. Returns the newly-created issue's
    /// `I_…` node ID — the caller needs this to populate `RemoteRef.node_id`
    /// on the local task so future GraphQL mutations have an address.
    /// Fires when an orphan task gets `--repo` attached via `rl task edit`.
    async fn convert_draft_to_issue(
        &self,
        item_node_id: &str,
        repo_node_id: &str,
    ) -> PortResult<String>;

    /// Set an item's single-select Status field. Works on both draft items
    /// and issue-backed items.
    async fn set_status(
        &self,
        project_node_id: &str,
        item_node_id: &str,
        status_field_id: &str,
        option_id: &str,
    ) -> PortResult<()>;

    /// Poll a project for items changed since `since` matching `query`
    /// (e.g. `"is:open"`). Returns both issue-backed items and drafts;
    /// `RemoteProjectItem.issue_node_id` is `None` for drafts.
    async fn poll_project_items(
        &self,
        project_node_id: &str,
        since: Timestamp,
        query: &str,
    ) -> PortResult<Vec<RemoteProjectItem>>;
}

#[async_trait]
pub trait ProjectRepository: Send + Sync {
    async fn save(&self, project: &Project) -> PortResult<()>;
    async fn get(&self, id: ProjectId) -> PortResult<Project>;
    async fn list_by_workspace(&self, ws: WorkspaceId) -> PortResult<Vec<Project>>;
    /// All locally-known projects, irrespective of workspace membership.
    /// Backs `rl project list` and the `owner/number` resolver in the
    /// application layer (projects have no UNIQUE index on `(owner, number)`,
    /// so the resolver scans this set).
    async fn list_all(&self) -> PortResult<Vec<Project>>;
    async fn delete(&self, id: ProjectId) -> PortResult<()>;
}
