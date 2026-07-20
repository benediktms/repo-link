//! Remote project provider port and its DTOs, plus the project repository.

use async_trait::async_trait;
use domain_core::{ProjectId, Timestamp, WorkspaceId};
use domain_project::{OrgIssueTypeRegistry, Project};

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
    /// Every retained single-select field on the board (RFC 0006 D2). The
    /// adapter no longer collapses to a single Status field — Status-vs-other
    /// selection is a domain concern applied at link time (named matching).
    pub fields: Vec<RemoteProjectField>,
}

#[derive(Clone, Debug)]
pub struct RemoteProjectField {
    pub field_id: String,
    pub name: String,
    pub options: Vec<RemoteProjectFieldOption>,
}

#[derive(Clone, Debug)]
pub struct RemoteProjectFieldOption {
    pub option_id: String,
    pub name: String,
    pub ordinal: u32,
}

/// One org-level native issue type as returned by the provider (RFC 0006
/// D5/D8). The org-decoupled counterpart of [`RemoteProjectFieldOption`]:
/// issue types are an organization catalog, not a per-board field.
#[derive(Clone, Debug)]
pub struct RemoteIssueType {
    pub issue_type_id: String,
    pub name: String,
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
    /// link` to learn the project's retained single-select fields (id + name +
    /// option catalog); Status-vs-other classification happens in the domain.
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

    /// Convert a draft item to a real issue in the filing repo. New callers
    /// pass its `github.com/<owner>/<repo>` canonical; adapters also accept a
    /// repository node ID so persisted entries from older versions can drain.
    /// The item
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
        repo_ref: &str,
    ) -> PortResult<(String, u64)>;

    /// Set an item's single-select field to `option_id` (RFC 0006 D4 — the
    /// generalized form of the old Status-only `set_status`). Field-agnostic
    /// on the wire: `field_id` names whichever single-select is being written
    /// — the board Status field or the Priority field share this one method.
    /// Works on both draft items and issue-backed items. Returns the
    /// **applied** `option_id` read back from the mutation response — the
    /// drainer compares it against the sent `option_id` to detect a conflict
    /// (RFC 0004 D5 for Status; Priority deliberately does NOT flip the task
    /// to `Conflict` on a mismatch, see the drainer's `SetProjectPriority`
    /// arm). An otherwise-successful mutation whose response omits the
    /// single-select value is an error (the caller treats it as
    /// transient/retry), not a silent confirmation.
    async fn set_single_select_option(
        &self,
        project_node_id: &str,
        item_node_id: &str,
        field_id: &str,
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

    /// Fetch the owner's org-level native issue-type catalog (RFC 0006
    /// D5/D8). Called at `rl project link` time to (re)build the org registry
    /// cache. Per D8 a user-owned owner (personal account) or an org with the
    /// feature disabled has no types — that is an empty vec, NEVER an error;
    /// the GitHub adapter reaches this via `repositoryOwner(login:)` + an
    /// `... on Organization` fragment so a non-org owner deserializes to an
    /// empty set rather than a GraphQL error.
    ///
    /// The default returns an empty catalog so the non-GitHub implementors
    /// (the in-memory fixture + the daemon's project-provider test doubles)
    /// compile untouched; only [`crate`]'s GitHub adapter overrides it. A
    /// maintainer tidying this default away must add overrides to those
    /// doubles.
    async fn fetch_org_issue_types(&self, _owner_login: &str) -> PortResult<Vec<RemoteIssueType>> {
        Ok(Vec::new())
    }

    /// Set (or clear) an issue's native "Type" field via GraphQL
    /// `updateIssue(input: { id, issueTypeId })` (RFC 0006 §0 A1 / #228).
    /// `issue_type_id` is an `OrgIssueType::issue_type_id` (`IT_…`) resolved
    /// by the caller against the org registry; `None` clears the type.
    ///
    /// Deliberately OFF the issue-mirror axis: this is a dedicated GraphQL
    /// projection, not a REST PATCH field — octocrab's issue builder has no
    /// `issue_type` slot, so type can never ride `update_remote`/`MirrorPatch`.
    /// No read-back comparison: unlike `set_single_select_option`, the drainer
    /// never flips the task to `Conflict` on this projection (see the
    /// `OutboxMutation::SetIssueType` doc) — `Ok(())` is success regardless of
    /// what the remote echoes.
    ///
    /// The default returns `Ok(())` so the non-GitHub implementors (the
    /// in-memory fixture + the daemon's project-provider test doubles)
    /// compile untouched; only [`crate`]'s GitHub adapter overrides it. A
    /// maintainer tidying this default away must add overrides to those
    /// doubles.
    async fn set_issue_type(
        &self,
        _issue_node_id: &str,
        _issue_type_id: Option<&str>,
    ) -> PortResult<()> {
        Ok(())
    }
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

/// Persistence for the org-level native issue-type registry (RFC 0006 D5).
///
/// Kept as its own trait — separate from [`ProjectRepository`] — because the
/// registry is org-scoped and decoupled from any board (D5): one owner's types
/// are shared across every project it owns, so they do not belong on the
/// per-project aggregate.
#[async_trait]
pub trait OrgIssueTypeRepository: Send + Sync {
    /// Replace the owner's cached registry wholesale.
    async fn save(&self, registry: &OrgIssueTypeRegistry) -> PortResult<()>;
    /// Load the owner's cached registry. An owner with no cached rows returns
    /// an empty registry (`is_available() == false`), NEVER `NotFound` — the
    /// D8 availability signal is an empty set, not an error.
    async fn get(&self, owner_login: &str) -> PortResult<OrgIssueTypeRegistry>;
}
