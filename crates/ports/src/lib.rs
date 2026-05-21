//! ports — async trait contracts between application and infrastructure.

use std::path::Path;

use async_trait::async_trait;
use domain_core::{RepoId, TaskId, Timestamp, WorkspaceId};
use domain_repo::RepoBinding;
use domain_task::{Task, TaskState};
use domain_workspace::Workspace;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PortError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("backend failure: {0}")]
    Backend(String),

    #[error("network failure: {0}")]
    Network(String),
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
    async fn delete(&self, id: RepoId) -> PortResult<()>;
}

// ---------- Task repository -----------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct TaskFilter {
    pub workspace_id: Option<WorkspaceId>,
    pub repo_id: Option<RepoId>,
    pub state: Option<TaskState>,
    pub include_archived: bool,
}

#[async_trait]
pub trait TaskRepository: Send + Sync {
    async fn save(&self, task: &Task) -> PortResult<()>;
    async fn get(&self, id: TaskId) -> PortResult<Task>;
    async fn list(&self, filter: TaskFilter) -> PortResult<Vec<Task>>;
    async fn delete(&self, id: TaskId) -> PortResult<()>;
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

#[derive(Clone, Debug)]
pub struct RemoteTaskUpdate<'a> {
    pub canonical_repo: &'a str,
    pub remote_id: &'a str,
    pub title: Option<&'a str>,
    pub body: Option<&'a str>,
    pub closed: Option<bool>,
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
