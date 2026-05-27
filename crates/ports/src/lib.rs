//! ports — async trait contracts between application and infrastructure.

use std::path::Path;

use async_trait::async_trait;
use domain_core::{RepoId, TaskId, Timestamp, WorkspaceId};
use domain_repo::RepoBinding;
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

#[async_trait]
pub trait RemoteTaskProvider: Send + Sync {
    async fn create_remote(&self, cmd: RemoteTaskCreate<'_>) -> PortResult<RemoteTaskSnapshot>;
    async fn update_remote(&self, cmd: RemoteTaskUpdate<'_>) -> PortResult<RemoteTaskSnapshot>;
    async fn fetch_remote(
        &self,
        canonical_repo: &str,
        remote_id: &str,
    ) -> PortResult<RemoteTaskSnapshot>;
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
