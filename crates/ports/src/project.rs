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

/// One [`RemoteProjectProvider::poll_project_items`] result, carrying the items
/// *and* a truthful partiality flag. `truncated` is set by the provider when it
/// could not enumerate the whole connection (the page cap was hit), so the
/// caller must not infer completeness from `items.len()`: an adapter that drops
/// unmodelled nodes (PRs, hidden content) can return fewer items than a naive
/// page-size heuristic would expect even on a truncated read. The poller relies
/// on this flag to decide whether to advance its per-project watermark.
#[derive(Clone, Debug)]
pub struct PollPage {
    pub items: Vec<RemoteProjectItem>,
    /// `true` when the provider could not see the whole result set (e.g. it hit
    /// its pagination cap). The poller treats such a page as partial and does
    /// NOT advance the watermark, so the next cycle refetches the same window.
    pub truncated: bool,
}

#[async_trait]
pub trait RemoteProjectProvider: Send + Sync {
    /// Resolve `owner/number` → project schema. Called once per `rl project
    /// link` to learn the project's Status field id and option catalog.
    async fn fetch_project(&self, owner: &str, number: u64) -> PortResult<RemoteProjectSnapshot>;

    /// Attach an existing issue to a project. Returns the new item's
    /// `PVTI_…` node ID. Idempotent in practice because it relies on
    /// GitHub's server-side idempotency of `addProjectV2ItemById` — re-adding
    /// the same content returns the existing item rather than duplicating it;
    /// the adapter does not itself dedupe.
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
    /// `I_…` node ID **and** its REST `number` — the caller needs both to
    /// populate a fully-addressable `RemoteRef` (`remote_id` = the number,
    /// `node_id` = the node ID) on the local task: the node id addresses
    /// GraphQL mutations, the number addresses REST `UpdateRemote`. Returning
    /// the number here is what lets the write-back avoid persisting an
    /// issue-backed `RemoteRef` with an empty `remote_id` (#54). Fires when an
    /// orphan task gets `--repo` attached via `rl task edit`.
    async fn convert_draft_to_issue(
        &self,
        item_node_id: &str,
        repo_node_id: &str,
    ) -> PortResult<(String, u64)>;

    /// Set an item's single-select Status field. Works on both draft items
    /// and issue-backed items. Returns the **applied** `option_id` read back
    /// from the mutation response — the drainer compares it against the sent
    /// `option_id` to detect a project-status conflict (RFC 0004 D5). An
    /// otherwise-successful mutation whose response omits the single-select
    /// value is an error (the caller treats it as transient/retry), not a
    /// silent confirmation.
    async fn set_status(
        &self,
        project_node_id: &str,
        item_node_id: &str,
        status_field_id: &str,
        option_id: &str,
    ) -> PortResult<String>;

    /// Poll a project for items matching `query`, a Projects-v2 filter (#208).
    /// `ProjectV2.items(query:)` has no `updated:` qualifier, so there is no
    /// server-side time delta: an empty `query` enumerates the whole board (the
    /// status-reconciliation poller passes empty and applies its watermark
    /// client-side). Returns both issue-backed items and drafts;
    /// `RemoteProjectItem.issue_node_id` is `None` for drafts.
    ///
    /// `status_field_id` is the project's chosen Status field (`PVTSSF_…`, as
    /// resolved by [`Self::fetch_project`] and persisted on the project). The
    /// item's status option is read from *that* field by id — not by the
    /// literal field name "Status", which would miss boards whose single-select
    /// field is named anything else.
    ///
    /// Returns a [`PollPage`]: the items plus a `truncated` flag the provider
    /// sets when it could not enumerate the whole result set. The caller must
    /// trust that flag rather than inferring partiality from the item count —
    /// the count is lossy because unmodelled nodes are silently dropped.
    async fn poll_project_items(
        &self,
        project_node_id: &str,
        status_field_id: &str,
        query: &str,
    ) -> PortResult<PollPage>;
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
