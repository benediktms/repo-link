//! testing-fixtures — in-memory port impls + deterministic clock for tests.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use domain_core::{RepoId, TaskId, Timestamp, WorkspaceId};
use domain_repo::RepoBinding;
use domain_task::Task;
use domain_workspace::Workspace;
use dto_events::EventEnvelope;
use ports::{
    Clock, EventSink, FilesystemProbe, PortError, PortResult, RepoBindingRepository, TaskFilter,
    TaskRepository, WorkspaceRepository,
};

// ---------- Clock --------------------------------------------------------

pub struct FixedClock {
    instant: Timestamp,
}

impl FixedClock {
    pub fn new_epoch() -> Self {
        Self {
            instant: Timestamp::from_utc(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
        }
    }

    pub fn at(instant: Timestamp) -> Self {
        Self { instant }
    }
}

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        self.instant
    }
}

// ---------- Workspace repository ------------------------------------------

#[derive(Default)]
pub struct InMemoryWorkspaceRepository {
    inner: Mutex<HashMap<WorkspaceId, Workspace>>,
}

impl InMemoryWorkspaceRepository {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl WorkspaceRepository for InMemoryWorkspaceRepository {
    async fn save(&self, workspace: &Workspace) -> PortResult<()> {
        self.inner
            .lock()
            .unwrap()
            .insert(workspace.id, workspace.clone());
        Ok(())
    }

    async fn get(&self, id: WorkspaceId) -> PortResult<Workspace> {
        self.inner
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| PortError::NotFound(format!("workspace {id}")))
    }

    async fn find_by_name(&self, name: &str) -> PortResult<Option<Workspace>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .values()
            .find(|w| w.name.as_str() == name)
            .cloned())
    }

    async fn list(&self, include_archived: bool) -> PortResult<Vec<Workspace>> {
        let g = self.inner.lock().unwrap();
        let mut rows: Vec<_> = g
            .values()
            .filter(|w| {
                include_archived
                    || !matches!(
                        w.status,
                        domain_workspace::WorkspaceStatus::Archived
                            | domain_workspace::WorkspaceStatus::Deleted
                    )
            })
            .cloned()
            .collect();
        rows.sort_by_key(|w| w.created_at);
        Ok(rows)
    }

    async fn delete(&self, id: WorkspaceId) -> PortResult<()> {
        self.inner.lock().unwrap().remove(&id);
        Ok(())
    }
}

// ---------- Repo binding repository ---------------------------------------

#[derive(Default)]
pub struct InMemoryRepoBindingRepository {
    inner: Mutex<HashMap<RepoId, RepoBinding>>,
}

impl InMemoryRepoBindingRepository {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl RepoBindingRepository for InMemoryRepoBindingRepository {
    async fn save(&self, binding: &RepoBinding) -> PortResult<()> {
        self.inner
            .lock()
            .unwrap()
            .insert(binding.id, binding.clone());
        Ok(())
    }

    async fn get(&self, id: RepoId) -> PortResult<RepoBinding> {
        self.inner
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| PortError::NotFound(format!("repo {id}")))
    }

    async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> PortResult<Vec<RepoBinding>> {
        let g = self.inner.lock().unwrap();
        let mut rows: Vec<_> = g
            .values()
            .filter(|b| b.workspace_id == workspace_id)
            .cloned()
            .collect();
        rows.sort_by_key(|b| b.created_at);
        Ok(rows)
    }

    async fn find_by_canonical_url(
        &self,
        workspace_id: WorkspaceId,
        canonical_url: &str,
    ) -> PortResult<Option<RepoBinding>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .values()
            .find(|b| b.workspace_id == workspace_id && b.canonical_url == canonical_url)
            .cloned())
    }

    async fn delete(&self, id: RepoId) -> PortResult<()> {
        self.inner.lock().unwrap().remove(&id);
        Ok(())
    }
}

// ---------- Task repository -----------------------------------------------

#[derive(Default)]
pub struct InMemoryTaskRepository {
    inner: Mutex<HashMap<TaskId, Task>>,
}

impl InMemoryTaskRepository {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl TaskRepository for InMemoryTaskRepository {
    async fn save(&self, task: &Task) -> PortResult<()> {
        self.inner.lock().unwrap().insert(task.id, task.clone());
        Ok(())
    }

    async fn get(&self, id: TaskId) -> PortResult<Task> {
        self.inner
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| PortError::NotFound(format!("task {id}")))
    }

    async fn list(&self, filter: TaskFilter) -> PortResult<Vec<Task>> {
        let g = self.inner.lock().unwrap();
        let mut rows: Vec<_> = g
            .values()
            .filter(|t| match filter.workspace_id {
                Some(w) => t.workspace_id == w,
                None => true,
            })
            .filter(|t| match filter.repo_id {
                Some(r) => t.repo_id == Some(r),
                None => true,
            })
            .filter(|t| match filter.state {
                Some(s) => t.state == s,
                None => true,
            })
            .filter(|t| filter.include_archived || t.state != domain_task::TaskState::Archived)
            .cloned()
            .collect();
        rows.sort_by_key(|t| t.created_at);
        Ok(rows)
    }

    async fn delete(&self, id: TaskId) -> PortResult<()> {
        self.inner.lock().unwrap().remove(&id);
        Ok(())
    }
}

// ---------- Filesystem probe ----------------------------------------------

#[derive(Default)]
pub struct StubFilesystemProbe {
    existing: Mutex<Vec<PathBuf>>,
    worktrees: Mutex<Vec<PathBuf>>,
}

impl StubFilesystemProbe {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_path(self, path: impl Into<PathBuf>) -> Self {
        self.existing.lock().unwrap().push(path.into());
        self
    }

    pub fn with_worktree(self, path: impl Into<PathBuf>) -> Self {
        let p = path.into();
        self.existing.lock().unwrap().push(p.clone());
        self.worktrees.lock().unwrap().push(p);
        self
    }

    pub fn remove(&self, path: &Path) {
        self.existing.lock().unwrap().retain(|p| p != path);
        self.worktrees.lock().unwrap().retain(|p| p != path);
    }
}

#[async_trait]
impl FilesystemProbe for StubFilesystemProbe {
    async fn path_exists(&self, path: &Path) -> PortResult<bool> {
        Ok(self.existing.lock().unwrap().iter().any(|p| p == path))
    }

    async fn is_git_worktree(&self, path: &Path) -> PortResult<bool> {
        Ok(self.worktrees.lock().unwrap().iter().any(|p| p == path))
    }
}

// ---------- Event sink ----------------------------------------------------

#[derive(Default)]
pub struct CapturingEventSink {
    inner: Mutex<Vec<EventEnvelope>>,
}

impl CapturingEventSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<EventEnvelope> {
        self.inner.lock().unwrap().clone()
    }
}

#[async_trait]
impl EventSink for CapturingEventSink {
    async fn record(&self, envelope: EventEnvelope) -> PortResult<()> {
        self.inner.lock().unwrap().push(envelope);
        Ok(())
    }
}

#[cfg(test)]
mod smoke {
    use super::*;
    use domain_workspace::{Workspace, WorkspaceName};

    #[tokio::test]
    async fn workspace_save_and_list() {
        let repo = InMemoryWorkspaceRepository::new();
        let w = Workspace::new(WorkspaceName::new("a").unwrap(), None, true);
        repo.save(&w).await.unwrap();
        let listed = repo.list(false).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, w.id);
    }
}
