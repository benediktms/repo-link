//! ports — async trait contracts between application and infrastructure.

use std::path::Path;

use async_trait::async_trait;
use domain_core::{OutboxEntryId, ProjectId, RepoId, TaskId, Timestamp, WorkspaceId};
use domain_project::Project;
use domain_repo::RepoBinding;
use domain_sync::OutboxEntry;
use domain_task::{SnapshotSource, SyncState, Task, TaskSnapshot, TaskStatus};
use domain_workspace::Workspace;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PortError {
    #[error("not found: {0}")]
    NotFound(String),

    /// A uniqueness violation. `target` names the logical constraint the
    /// backend reported (e.g. `"tasks.hash"`, `"repos.prefix"`) when it
    /// can — adapters translate their native error into this structured
    /// form so the application layer can drive retry logic off the
    /// target instead of substring-matching backend-specific message
    /// text. `None` when the backend gives no usable target.
    #[error("conflict{}: {message}", .target.as_deref().map(|t| format!(" on {t}")).unwrap_or_default())]
    Conflict {
        target: Option<String>,
        message: String,
    },

    #[error("backend failure: {0}")]
    Backend(String),

    #[error("network failure: {0}")]
    Network(String),

    /// The remote issue at `from_canonical#from_remote_id` was administratively
    /// transferred to `to_canonical#to_remote_id` (GitHub returned 301 with a
    /// `Location` header). Adapters surface this *typed* error instead of a
    /// raw network failure so callers can offer a verified re-link rather than
    /// asking the user to diagnose an opaque HTTP code.
    #[error(
        "remote issue {from_canonical}#{from_remote_id} moved to {to_canonical}#{to_remote_id}"
    )]
    IssueMoved {
        from_canonical: String,
        from_remote_id: String,
        to_canonical: String,
        to_remote_id: String,
    },
}

impl PortError {
    /// The logical target of a uniqueness [`PortError::Conflict`]
    /// (e.g. `"tasks.hash"`, `"repos.prefix"`), if the backend reported
    /// one. Returns `None` for non-conflict errors or conflicts without
    /// a target.
    pub fn conflict_target(&self) -> Option<&str> {
        match self {
            PortError::Conflict {
                target: Some(t), ..
            } => Some(t.as_str()),
            _ => None,
        }
    }
}

pub type PortResult<T> = std::result::Result<T, PortError>;

// ---------- Workspace repository -----------------------------------------

#[async_trait]
pub trait WorkspaceRepository: Send + Sync {
    async fn save(&self, workspace: &Workspace) -> PortResult<()>;
    async fn get(&self, id: WorkspaceId) -> PortResult<Workspace>;
    async fn find_by_name(&self, name: &str) -> PortResult<Option<Workspace>>;
    async fn list(&self, include_archived: bool) -> PortResult<Vec<Workspace>>;
    async fn delete(&self, id: WorkspaceId) -> PortResult<()>;
}

// ---------- Repo binding repository --------------------------------------

#[async_trait]
pub trait RepoBindingRepository: Send + Sync {
    async fn save(&self, binding: &RepoBinding) -> PortResult<()>;
    async fn get(&self, id: RepoId) -> PortResult<RepoBinding>;
    async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> PortResult<Vec<RepoBinding>>;
    async fn find_by_canonical_url(
        &self,
        workspace_id: WorkspaceId,
        canonical_url: &str,
    ) -> PortResult<Option<RepoBinding>>;
    /// Look up a binding by its globally-unique `prefix`. Used by the
    /// repo locator path so callers can pass `--repo rpl` (or use
    /// `rpl-ak7` for tasks and reuse the prefix half here) instead of a
    /// UUID.
    async fn find_by_prefix(&self, prefix: &str) -> PortResult<Option<RepoBinding>>;
    async fn delete(&self, id: RepoId) -> PortResult<()>;
}

// ---------- Task repository -----------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct TaskFilter {
    pub workspace_id: Option<WorkspaceId>,
    pub repo_id: Option<RepoId>,
    /// Filter by lifecycle status. When `None`, callers usually want
    /// non-archived rows only — see `include_archived`.
    pub status: Option<TaskStatus>,
    /// Filter by sync state.
    pub sync_state: Option<SyncState>,
    /// When `status` is `None`, include `Archived` rows. Ignored if
    /// `status` is set explicitly.
    pub include_archived: bool,
}

#[async_trait]
pub trait TaskRepository: Send + Sync {
    /// Persist `task` and append a new row to its snapshot history,
    /// tagged with `source`. The adapter assigns the next monotonic
    /// `version`. Both writes are committed in a single transaction.
    async fn save(&self, task: &Task, source: SnapshotSource) -> PortResult<()>;
    async fn get(&self, id: TaskId) -> PortResult<Task>;
    async fn list(&self, filter: TaskFilter) -> PortResult<Vec<Task>>;
    /// Look up a task by its globally-unique `hash`. Used by the
    /// friendly-ID resolver so callers can pass a bare hash (`ak7`) or
    /// the prefix half of a composite (`rlk-ak7`) instead of a UUID.
    async fn find_by_hash(&self, hash: &str) -> PortResult<Option<Task>>;
    /// Look up the task mirroring a given remote issue within a repo
    /// (`repo_id` + `provider` + `remote_id`). Scoped by repo because remote
    /// issue numbers are only unique per repo (GitHub `repoA#123` ≠
    /// `repoB#123`). Used by `sync import` to skip already-tracked issues.
    async fn find_by_remote(
        &self,
        repo_id: RepoId,
        provider: &str,
        remote_id: &str,
    ) -> PortResult<Option<Task>>;
    /// Replace the task's *synced* comments with `comments` (always
    /// remote-backed — taking [`RemoteComment`] rather than `TaskComment`
    /// makes pending input unrepresentable), leaving any pending local-only
    /// comments untouched. Writes only the `task_comments` table — never a
    /// snapshot — so mirroring remote comments doesn't perturb sync state.
    async fn replace_comments(&self, task_id: TaskId, comments: &[RemoteComment])
    -> PortResult<()>;
    /// Append a single pending (local-only) comment, stored with the empty
    /// `remote_comment_id` sentinel. Writes only the `task_comments` table —
    /// never a snapshot — so adding a comment never perturbs sync state
    /// (pending comments are a separate outbound axis from title/body drift).
    async fn add_pending_comment(
        &self,
        task_id: TaskId,
        author: &str,
        body: &str,
        created_at: Timestamp,
    ) -> PortResult<()>;
    /// Promote a task's pending comments to synced after a successful remote
    /// push: deletes the rows in `drained_local_ids` and inserts `pushed` as
    /// synced rows. Writes only `task_comments`, never a snapshot.
    ///
    /// Identity-aware so the drain can't race-delete a pending comment that
    /// was added between the caller reading the task and this call: only the
    /// rows whose surrogate id was actually pushed are removed.
    async fn mark_comments_pushed(
        &self,
        task_id: TaskId,
        drained_local_ids: &[String],
        pushed: &[RemoteComment],
    ) -> PortResult<()>;
    /// Count pending (local-only) comments per task across a workspace, so
    /// `query unsynced` can surface comment-only outbound work without loading
    /// every task's comments (`list` deliberately skips them). Returns only
    /// tasks with at least one pending comment.
    async fn pending_comment_counts(
        &self,
        workspace_id: WorkspaceId,
    ) -> PortResult<std::collections::HashMap<TaskId, usize>>;
    async fn delete(&self, id: TaskId) -> PortResult<()>;
}

/// History queries over [`TaskSnapshot`] rows. Reads only — appends are
/// the side-effect of [`TaskRepository::save`] (so the snapshot table and
/// the task projection can't drift apart).
#[async_trait]
pub trait TaskSnapshotRepository: Send + Sync {
    /// All snapshots for a task, oldest version first.
    async fn list(&self, task_id: TaskId) -> PortResult<Vec<TaskSnapshot>>;

    /// Fetch a specific version. Returns `NotFound` if the version
    /// doesn't exist.
    async fn get(&self, task_id: TaskId, version: u64) -> PortResult<TaskSnapshot>;
}

// ---------- Remote task provider (GitHub etc.) ---------------------------

#[derive(Clone, Debug)]
pub struct RemoteTaskCreate<'a> {
    pub canonical_repo: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub assignees: &'a [String],
    pub labels: &'a [String],
}

/// Why a remote task is changing state. Providers that don't model this
/// (GitLab, custom backends) can silently drop it. Names mirror GitHub's
/// `state_reason` vocab because that's the most expressive enumeration
/// currently in the wild; adapters map to their wire format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteStateReason {
    /// Work finished as planned.
    Completed,
    /// Won't be done — dropped, abandoned, deferred indefinitely.
    NotPlanned,
    /// Closed because it's a duplicate of another task.
    Duplicate,
    /// Closed → open transition.
    Reopened,
}

#[derive(Clone, Debug)]
pub struct RemoteTaskUpdate<'a> {
    pub canonical_repo: &'a str,
    pub remote_id: &'a str,
    pub title: Option<&'a str>,
    pub body: Option<&'a str>,
    pub closed: Option<bool>,
    /// Annotates a state transition. Meaningful with `closed = Some(true)`
    /// (Completed / NotPlanned / Duplicate) or when reopening
    /// (`Reopened`). Adapters ignore the field if their backend has no
    /// equivalent concept.
    pub state_reason: Option<RemoteStateReason>,
}

#[derive(Clone, Debug)]
pub struct RemoteTaskSnapshot {
    pub remote_id: String,
    pub title: String,
    pub body: String,
    pub closed: bool,
    pub updated_at: Timestamp,
    pub assignees: Vec<String>,
    pub labels: Vec<String>,
}

/// One sub-issue returned by [`RemoteTaskProvider::fetch_sub_issues`], paired
/// with the canonical repo it actually lives in. A sub-issue can belong to a
/// different repo than its parent, so the canonical is carried here (rather
/// than widening [`RemoteTaskSnapshot`]) to let the import orchestrator detect
/// and skip cross-repo children.
#[derive(Clone, Debug)]
pub struct RemoteChildIssue {
    pub canonical_repo: String,
    pub snapshot: RemoteTaskSnapshot,
}

/// A comment fetched from a remote issue. Always carries a remote id (the
/// provider assigns one on create); the local-only / pending case is
/// represented by [`domain_task::TaskComment::remote_id`] being `None`.
#[derive(Clone, Debug)]
pub struct RemoteComment {
    pub remote_id: String,
    pub author: String,
    pub body: String,
    pub created_at: Timestamp,
}

#[async_trait]
pub trait RemoteTaskProvider: Send + Sync {
    async fn create_remote(&self, cmd: RemoteTaskCreate<'_>) -> PortResult<RemoteTaskSnapshot>;
    async fn update_remote(&self, cmd: RemoteTaskUpdate<'_>) -> PortResult<RemoteTaskSnapshot>;
    async fn fetch_remote(
        &self,
        canonical_repo: &str,
        remote_id: &str,
    ) -> PortResult<RemoteTaskSnapshot>;

    /// List the direct (one level) sub-issues of a remote task. Providers
    /// without a sub-issue concept inherit the default empty result, so only
    /// adapters that support it (GitHub) need to override. Recursion into
    /// grandchildren is the caller's job.
    async fn fetch_sub_issues(
        &self,
        _canonical_repo: &str,
        _remote_id: &str,
    ) -> PortResult<Vec<RemoteChildIssue>> {
        Ok(Vec::new())
    }

    /// List the comments on a remote task, oldest first. Providers without a
    /// comment concept inherit the default empty result; GitHub overrides.
    async fn fetch_comments(
        &self,
        _canonical_repo: &str,
        _remote_id: &str,
    ) -> PortResult<Vec<RemoteComment>> {
        Ok(Vec::new())
    }

    /// Create a comment on a remote task and return it with its provider-
    /// assigned id/author/timestamp. Required (no default): a write has no
    /// sensible no-op fallback, so each provider must implement it explicitly.
    async fn create_comment(
        &self,
        canonical_repo: &str,
        remote_id: &str,
        body: &str,
    ) -> PortResult<RemoteComment>;

    /// Probe the remote for a transferred-issue redirect. Returns
    /// `Some((to_canonical_repo, to_remote_id))` if the provider reports the
    /// task at `(canonical_repo, remote_id)` has been moved, `None` if the
    /// task is still at the supplied address. Used by `rl task link --relink`
    /// to verify a user-supplied URL is GitHub's actual redirect target
    /// before rewriting the task's remote identity. Providers without a
    /// transfer concept inherit the default `Ok(None)`.
    async fn discover_move_target(
        &self,
        _canonical_repo: &str,
        _remote_id: &str,
    ) -> PortResult<Option<(String, String)>> {
        Ok(None)
    }

    /// List issues in `canonical_repo` whose `updatedAt` is at or after
    /// `since`. Backs the REST polling fallback used by binding-only
    /// (projectless) workspaces — see RFC 0001 §3 D4. Providers without
    /// a since-filter inherit the default empty result, so only adapters
    /// that support it (GitHub) need to override.
    async fn list_changed_since(
        &self,
        _canonical_repo: &str,
        _since: Timestamp,
    ) -> PortResult<Vec<RemoteTaskSnapshot>> {
        Ok(Vec::new())
    }
}

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

#[async_trait]
pub trait OutboxRepository: Send + Sync {
    async fn enqueue(&self, entry: &OutboxEntry) -> PortResult<()>;
    /// Atomically claim the oldest `pending` entry and mark it `inflight`.
    /// `None` when the queue is empty — the drainer can sleep until the
    /// next tick.
    async fn next_pending(&self) -> PortResult<Option<OutboxEntry>>;
    async fn mark_succeeded(&self, id: OutboxEntryId) -> PortResult<()>;
    async fn mark_failed(&self, id: OutboxEntryId, error: &str) -> PortResult<()>;
    async fn list_pending(&self, task_id: TaskId) -> PortResult<Vec<OutboxEntry>>;
}

// ---------- Filesystem probe ---------------------------------------------

#[async_trait]
pub trait FilesystemProbe: Send + Sync {
    async fn path_exists(&self, path: &Path) -> PortResult<bool>;
    async fn is_git_worktree(&self, path: &Path) -> PortResult<bool>;
}

// ---------- Clock --------------------------------------------------------

/// Injected to keep reconciliation deterministic in tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

// ---------- Event sink ---------------------------------------------------

#[async_trait]
pub trait EventSink: Send + Sync {
    async fn record(&self, envelope: dto_events::EventEnvelope) -> PortResult<()>;
}
