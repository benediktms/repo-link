//! application-task — Task CRUD + lifecycle orchestration.

use std::sync::Arc;

use domain_core::{IdParseError, RepoId, TaskId, WorkspaceId};
use domain_task::{Priority, RelationKind, SnapshotSource, SyncState, Task, TaskStatus};
use dto_shared::{
    AddTaskRelationCmd, CreateTaskCmd, ListTasksQuery, RemoteRefDto, TaskDto, TaskRelationDto,
    UpdateTaskCmd,
};
use ports::{PortError, TaskFilter, TaskRepository, TaskSnapshotRepository};
use serde::de::DeserializeOwned;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    Port(#[from] PortError),
    #[error(transparent)]
    Domain(#[from] domain_core::DomainError),
    #[error("invalid id: {0}")]
    BadId(String),
    #[error("invalid enum value for {field}: {value}")]
    BadEnum { field: &'static str, value: String },
}

impl From<IdParseError> for ServiceError {
    fn from(e: IdParseError) -> Self {
        Self::BadId(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, ServiceError>;

// ---------- TaskService ---------------------------------------------------

pub struct TaskService {
    repo: Arc<dyn TaskRepository>,
    snapshots: Arc<dyn TaskSnapshotRepository>,
}

impl TaskService {
    pub fn new(repo: Arc<dyn TaskRepository>, snapshots: Arc<dyn TaskSnapshotRepository>) -> Self {
        Self { repo, snapshots }
    }

    pub fn snapshots_repo(&self) -> &Arc<dyn TaskSnapshotRepository> {
        &self.snapshots
    }

    pub async fn create(&self, cmd: CreateTaskCmd) -> Result<TaskDto> {
        let workspace_id: WorkspaceId = cmd.workspace_id.parse()?;
        let repo_id = cmd
            .repo_id
            .as_deref()
            .map(|s| s.parse::<RepoId>())
            .transpose()?;
        let mut t = Task::new_draft(workspace_id, repo_id, cmd.title)?;
        if let Some(body) = cmd.body {
            t.set_body(body);
        }
        if let Some(p) = cmd.priority {
            t.set_priority(parse_enum::<Priority>("priority", &p)?);
        }
        self.repo.save(&t, SnapshotSource::LocalEdit).await?;
        Ok(task_to_dto(&t))
    }

    pub async fn show(&self, id: &str) -> Result<TaskDto> {
        let id: TaskId = id.parse()?;
        Ok(task_to_dto(&self.repo.get(id).await?))
    }

    pub async fn update(&self, cmd: UpdateTaskCmd) -> Result<TaskDto> {
        let id: TaskId = cmd.task_id.parse()?;
        let mut t = self.repo.get(id).await?;
        if let Some(title) = cmd.title {
            t.set_title(title)?;
        }
        if let Some(body) = cmd.body {
            t.set_body(body);
        }
        if let Some(p) = cmd.priority {
            t.set_priority(parse_enum::<Priority>("priority", &p)?);
        }
        if let Some(assignees) = cmd.assignees {
            t.set_assignees(assignees);
        }
        self.repo.save(&t, SnapshotSource::LocalEdit).await?;
        Ok(task_to_dto(&t))
    }

    pub async fn list(&self, query: ListTasksQuery) -> Result<Vec<TaskDto>> {
        let filter = TaskFilter {
            workspace_id: query
                .workspace_id
                .as_deref()
                .map(|s| s.parse::<WorkspaceId>())
                .transpose()?,
            repo_id: query
                .repo_id
                .as_deref()
                .map(|s| s.parse::<RepoId>())
                .transpose()?,
            status: query
                .status
                .as_deref()
                .map(|s| parse_enum::<TaskStatus>("status", s))
                .transpose()?,
            sync_state: query
                .sync_state
                .as_deref()
                .map(|s| parse_enum::<SyncState>("sync_state", s))
                .transpose()?,
            include_archived: query.include_archived,
        };
        let rows = self.repo.list(filter).await?;
        Ok(rows.iter().map(task_to_dto).collect())
    }

    // ---------- Sync transitions -----------------------------------------

    pub async fn stage_for_sync(&self, id: &str) -> Result<TaskDto> {
        self.transition(id, |t| t.stage_for_sync()).await
    }

    // ---------- Lifecycle transitions ------------------------------------

    pub async fn start(&self, id: &str) -> Result<TaskDto> {
        self.transition(id, |t| t.start()).await
    }

    pub async fn complete(&self, id: &str) -> Result<TaskDto> {
        self.transition(id, |t| t.complete()).await
    }

    pub async fn reopen(&self, id: &str) -> Result<TaskDto> {
        self.transition(id, |t| t.reopen()).await
    }

    pub async fn mark_blocked(&self, id: &str) -> Result<TaskDto> {
        self.transition(id, |t| t.mark_blocked()).await
    }

    pub async fn unblock(&self, id: &str) -> Result<TaskDto> {
        self.transition(id, |t| t.unblock()).await
    }

    pub async fn archive(&self, id: &str) -> Result<TaskDto> {
        self.transition(id, |t| t.archive()).await
    }

    pub async fn add_relation(&self, cmd: AddTaskRelationCmd) -> Result<TaskDto> {
        let id: TaskId = cmd.task_id.parse()?;
        let other: TaskId = cmd.other.parse()?;
        let kind = parse_enum::<RelationKind>("kind", &cmd.kind)?;
        let mut t = self.repo.get(id).await?;
        t.add_relation(kind, other);
        self.repo.save(&t, SnapshotSource::LocalEdit).await?;
        Ok(task_to_dto(&t))
    }

    pub async fn rollback(&self, id: &str, to_version: u64) -> Result<TaskDto> {
        let task_id: domain_core::TaskId = id.parse()?;
        let snapshot = self.snapshots.get(task_id, to_version).await?;
        let mut task = self.repo.get(task_id).await?;
        task.title = snapshot.title;
        task.body = snapshot.body;
        task.status = snapshot.status;
        task.sync = snapshot.sync_state;
        task.priority = snapshot.priority;
        task.assignees = snapshot.assignees;
        task.remote = snapshot.remote;
        task.reconcile_dirty_against_baseline();
        self.repo.save(&task, SnapshotSource::Rollback).await?;
        Ok(task_to_dto(&task))
    }

    async fn transition<F>(&self, id: &str, op: F) -> Result<TaskDto>
    where
        F: FnOnce(&mut Task) -> domain_core::Result<()>,
    {
        let id: TaskId = id.parse()?;
        let mut t = self.repo.get(id).await?;
        op(&mut t)?;
        self.repo.save(&t, SnapshotSource::LocalEdit).await?;
        Ok(task_to_dto(&t))
    }
}

// ---------- Mapping ------------------------------------------------------

fn enum_str<T: serde::Serialize>(t: &T) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

fn parse_enum<T: DeserializeOwned>(field: &'static str, value: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(value.to_string())).map_err(|_| {
        ServiceError::BadEnum {
            field,
            value: value.to_string(),
        }
    })
}

pub fn task_to_dto(t: &Task) -> TaskDto {
    TaskDto {
        id: t.id.to_string(),
        workspace_id: t.workspace_id.to_string(),
        repo_id: t.repo_id.map(|r| r.to_string()),
        title: t.title.clone(),
        body: t.body.clone(),
        status: enum_str(&t.status),
        sync_state: enum_str(&t.sync),
        priority: enum_str(&t.priority),
        assignees: t.assignees.clone(),
        remote: t.remote.as_ref().map(|r| RemoteRefDto {
            provider: r.provider.clone(),
            remote_id: r.remote_id.clone(),
        }),
        relations: t
            .relations
            .iter()
            .map(|r| TaskRelationDto {
                kind: enum_str(&r.kind),
                other: r.other.to_string(),
            })
            .collect(),
        created_at: t.created_at.into(),
        updated_at: t.updated_at.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ports::TaskSnapshotRepository;
    use testing_fixtures::{InMemoryTaskRepository, InMemoryTaskSnapshotRepository};

    fn svc() -> TaskService {
        let repo = Arc::new(InMemoryTaskRepository::new());
        let snaps: Arc<dyn TaskSnapshotRepository> =
            Arc::new(InMemoryTaskSnapshotRepository::linked_to(&repo));
        TaskService::new(repo, snaps)
    }

    fn ws_id() -> String {
        WorkspaceId::new().to_string()
    }

    #[tokio::test]
    async fn create_show_and_update_task() {
        let svc = svc();
        let dto = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "ship it".into(),
                body: Some("with feeling".into()),
                priority: Some("p1".into()),
            })
            .await
            .unwrap();
        assert_eq!(dto.status, "open");
        assert_eq!(dto.sync_state, "local_only");
        assert_eq!(dto.priority, "p1");
        let updated = svc
            .update(UpdateTaskCmd {
                task_id: dto.id.clone(),
                title: Some("ship it well".into()),
                body: None,
                priority: Some("p0".into()),
                assignees: Some(vec!["alice".into()]),
            })
            .await
            .unwrap();
        assert_eq!(updated.title, "ship it well");
        assert_eq!(updated.priority, "p0");
        assert_eq!(updated.assignees, vec!["alice".to_string()]);
    }

    #[tokio::test]
    async fn list_filters_independently_by_status_and_sync_state() {
        let svc = svc();
        let workspace = ws_id();
        let _open_localonly = svc
            .create(CreateTaskCmd {
                workspace_id: workspace.clone(),
                repo_id: None,
                title: "a".into(),
                body: None,
                priority: None,
            })
            .await
            .unwrap();
        let to_stage = svc
            .create(CreateTaskCmd {
                workspace_id: workspace.clone(),
                repo_id: None,
                title: "b".into(),
                body: None,
                priority: None,
            })
            .await
            .unwrap();
        svc.stage_for_sync(&to_stage.id).await.unwrap();

        // Both are status=Open. Filter by status returns both.
        let opens = svc
            .list(ListTasksQuery {
                status: Some("open".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(opens.len(), 2);

        // But sync state distinguishes them.
        let local_only = svc
            .list(ListTasksQuery {
                sync_state: Some("local_only".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(local_only.len(), 1);
        let staged = svc
            .list(ListTasksQuery {
                sync_state: Some("staged".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(staged.len(), 1);
    }

    #[tokio::test]
    async fn invalid_priority_returns_typed_error() {
        let svc = svc();
        let err = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "t".into(),
                body: None,
                priority: Some("p99".into()),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::BadEnum { field: "priority", .. }));
    }

    #[tokio::test]
    async fn add_relation_persists_to_repo() {
        let svc = svc();
        let a = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "a".into(),
                body: None,
                priority: None,
            })
            .await
            .unwrap();
        let b = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "b".into(),
                body: None,
                priority: None,
            })
            .await
            .unwrap();
        let updated = svc
            .add_relation(AddTaskRelationCmd {
                task_id: a.id.clone(),
                kind: "blocked_by".into(),
                other: b.id.clone(),
            })
            .await
            .unwrap();
        assert_eq!(updated.relations.len(), 1);
        assert_eq!(updated.relations[0].kind, "blocked_by");
        assert_eq!(updated.relations[0].other, b.id);
    }

    #[tokio::test]
    async fn lifecycle_start_complete_archive() {
        let svc = svc();
        let t = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "t".into(),
                body: None,
                priority: None,
            })
            .await
            .unwrap();
        let started = svc.start(&t.id).await.unwrap();
        assert_eq!(started.status, "in_progress");
        let done = svc.complete(&t.id).await.unwrap();
        assert_eq!(done.status, "done");
        let archived = svc.archive(&t.id).await.unwrap();
        assert_eq!(archived.status, "archived");
    }

    #[tokio::test]
    async fn block_and_unblock() {
        let svc = svc();
        let t = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "t".into(),
                body: None,
                priority: None,
            })
            .await
            .unwrap();
        let blocked = svc.mark_blocked(&t.id).await.unwrap();
        assert_eq!(blocked.status, "blocked");
        let unblocked = svc.unblock(&t.id).await.unwrap();
        assert_eq!(unblocked.status, "open");
    }

    #[tokio::test]
    async fn rollback_restores_original_title() {
        let svc = svc();
        // Create task — this writes version 1.
        let original = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "original title".into(),
                body: None,
                priority: None,
            })
            .await
            .unwrap();
        // Edit title — this writes version 2.
        svc.update(UpdateTaskCmd {
            task_id: original.id.clone(),
            title: Some("edited title".into()),
            body: None,
            priority: None,
            assignees: None,
        })
        .await
        .unwrap();
        // Rollback to version 1 — title should revert.
        let rolled_back = svc.rollback(&original.id, 1).await.unwrap();
        assert_eq!(rolled_back.title, "original title");
    }
}
