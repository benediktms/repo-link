//! application-sync — orchestrates remote promotion / push / pull.
//!
//! The service treats the local SQLite store as authoritative for draft
//! state and the remote (GitHub) as authoritative once a task has been
//! pushed. State transitions follow the explicit `TaskState` machine.

use std::sync::Arc;

use domain_core::{IdParseError, TaskId};
use domain_sync::{SyncDecision, SyncPolicy, decide};
use domain_task::{RemoteRef, Task, TaskState};
use dto_shared::{RemoteRefDto, SyncSummaryDto};
use ports::{
    PortError, RemoteTaskCreate, RemoteTaskProvider, RemoteTaskUpdate, RepoBindingRepository,
    TaskRepository,
};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error(transparent)]
    Port(#[from] PortError),
    #[error(transparent)]
    Domain(#[from] domain_core::DomainError),
    #[error("invalid id: {0}")]
    BadId(String),
    #[error("task is not bound to a repo")]
    NoRepo,
    #[error("task has no remote reference; promote it first")]
    NoRemote,
    #[error("manual merge required for task {0}")]
    ManualMerge(String),
}

impl From<IdParseError> for SyncError {
    fn from(e: IdParseError) -> Self {
        Self::BadId(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, SyncError>;

pub struct SyncService {
    tasks: Arc<dyn TaskRepository>,
    bindings: Arc<dyn RepoBindingRepository>,
    provider: Arc<dyn RemoteTaskProvider>,
    policy: SyncPolicy,
}

impl SyncService {
    pub fn new(
        tasks: Arc<dyn TaskRepository>,
        bindings: Arc<dyn RepoBindingRepository>,
        provider: Arc<dyn RemoteTaskProvider>,
    ) -> Self {
        Self {
            tasks,
            bindings,
            provider,
            policy: SyncPolicy::ManualMerge,
        }
    }

    pub fn with_policy(mut self, policy: SyncPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Stage (if needed) and promote a Draft/Staged task to a remote issue.
    pub async fn promote(&self, task_id: &str) -> Result<SyncSummaryDto> {
        let id: TaskId = task_id.parse()?;
        let mut task = self.tasks.get(id).await?;
        let canonical = self.canonical_for(&task).await?;
        let prev = task.state;

        if task.state == TaskState::Draft {
            task.stage_for_sync()?;
        }
        if task.state != TaskState::Staged {
            return Err(SyncError::Domain(domain_core::DomainError::transition(
                format!("cannot promote from {:?}", task.state),
            )));
        }

        let snap = self
            .provider
            .create_remote(RemoteTaskCreate {
                canonical_repo: &canonical,
                title: &task.title,
                body: &task.body,
                assignees: &task.assignees,
                labels: &[],
            })
            .await?;

        task.promote_to_remote(RemoteRef {
            provider: provider_label(&canonical),
            remote_id: snap.remote_id.clone(),
        })?;
        self.tasks.save(&task).await?;
        Ok(summary(&task, prev, SyncDecision::PushLocal))
    }

    /// Push local edits (state = DirtyLocal) to the remote.
    pub async fn push(&self, task_id: &str) -> Result<SyncSummaryDto> {
        let id: TaskId = task_id.parse()?;
        let mut task = self.tasks.get(id).await?;
        let canonical = self.canonical_for(&task).await?;
        let prev = task.state;
        let remote = task.remote.as_ref().ok_or(SyncError::NoRemote)?.clone();

        if !matches!(task.state, TaskState::DirtyLocal | TaskState::Staged) {
            return Err(SyncError::Domain(domain_core::DomainError::transition(
                format!("cannot push from {:?}", task.state),
            )));
        }

        self.provider
            .update_remote(RemoteTaskUpdate {
                canonical_repo: &canonical,
                remote_id: &remote.remote_id,
                title: Some(&task.title),
                body: Some(&task.body),
                closed: None,
            })
            .await?;

        task.mark_synced()?;
        self.tasks.save(&task).await?;
        Ok(summary(&task, prev, SyncDecision::PushLocal))
    }

    /// Pull the latest remote snapshot and reconcile.
    pub async fn pull(&self, task_id: &str) -> Result<SyncSummaryDto> {
        let id: TaskId = task_id.parse()?;
        let mut task = self.tasks.get(id).await?;
        let canonical = self.canonical_for(&task).await?;
        let remote = task.remote.as_ref().ok_or(SyncError::NoRemote)?.clone();
        let prev = task.state;

        let snap = self
            .provider
            .fetch_remote(&canonical, &remote.remote_id)
            .await?;
        let remote_changed = snap.updated_at.into_inner() > task.updated_at.into_inner();
        let decision = decide(task.state, remote_changed, self.policy);

        match decision {
            SyncDecision::Noop => {}
            SyncDecision::PullRemote => {
                apply_remote_snapshot(&mut task, &snap)?;
            }
            SyncDecision::PushLocal => {
                // Surfaced as a separate `push` call; pull keeps local intact.
            }
            SyncDecision::RequireManualMerge => {
                task.mark_conflicted()?;
                self.tasks.save(&task).await?;
                return Err(SyncError::ManualMerge(task_id.to_string()));
            }
        }

        self.tasks.save(&task).await?;
        Ok(summary(&task, prev, decision))
    }

    async fn canonical_for(&self, task: &Task) -> Result<String> {
        let repo_id = task.repo_id.ok_or(SyncError::NoRepo)?;
        let binding = self.bindings.get(repo_id).await?;
        Ok(binding.canonical_url)
    }
}

fn apply_remote_snapshot(task: &mut Task, snap: &ports::RemoteTaskSnapshot) -> Result<()> {
    task.title = snap.title.clone();
    task.set_body(snap.body.clone());
    task.assignees = snap.assignees.clone();
    if !matches!(task.state, TaskState::Synced) {
        // mark_synced rejects Draft/Archived/etc, so only call it for valid sources.
        if matches!(
            task.state,
            TaskState::Pushed | TaskState::DirtyLocal | TaskState::DirtyRemote
        ) {
            task.mark_synced()?;
        }
    }
    Ok(())
}

fn summary(task: &Task, prev: TaskState, decision: SyncDecision) -> SyncSummaryDto {
    SyncSummaryDto {
        task_id: task.id.to_string(),
        previous_state: enum_str(&prev),
        new_state: enum_str(&task.state),
        decision: enum_str(&decision),
        remote: task.remote.as_ref().map(|r| RemoteRefDto {
            provider: r.provider.clone(),
            remote_id: r.remote_id.clone(),
        }),
    }
}

fn provider_label(canonical: &str) -> String {
    if canonical.starts_with("github.com/") {
        "github".into()
    } else {
        "remote".into()
    }
}

fn enum_str<T: Serialize>(t: &T) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Utc;
    use domain_core::{Timestamp, WorkspaceId};
    use domain_repo::RepoBinding;
    use domain_task::Task;
    use ports::{PortResult, RemoteTaskSnapshot};
    use std::sync::Mutex;
    use testing_fixtures::{InMemoryRepoBindingRepository, InMemoryTaskRepository};

    #[derive(Default)]
    struct FakeProvider {
        last_create: Mutex<Option<String>>,
        last_update: Mutex<Option<(String, Option<String>)>>,
        fetch_returns: Mutex<Option<RemoteTaskSnapshot>>,
    }

    impl FakeProvider {
        fn set_fetch(&self, snap: RemoteTaskSnapshot) {
            *self.fetch_returns.lock().unwrap() = Some(snap);
        }
    }

    #[async_trait]
    impl RemoteTaskProvider for FakeProvider {
        async fn create_remote(
            &self,
            cmd: RemoteTaskCreate<'_>,
        ) -> PortResult<RemoteTaskSnapshot> {
            *self.last_create.lock().unwrap() = Some(cmd.title.to_string());
            Ok(RemoteTaskSnapshot {
                remote_id: "100".into(),
                title: cmd.title.into(),
                body: cmd.body.into(),
                closed: false,
                updated_at: Timestamp::from_utc(Utc::now()),
                assignees: cmd.assignees.to_vec(),
                labels: cmd.labels.to_vec(),
            })
        }

        async fn update_remote(
            &self,
            cmd: RemoteTaskUpdate<'_>,
        ) -> PortResult<RemoteTaskSnapshot> {
            *self.last_update.lock().unwrap() =
                Some((cmd.remote_id.into(), cmd.body.map(str::to_owned)));
            Ok(RemoteTaskSnapshot {
                remote_id: cmd.remote_id.into(),
                title: cmd.title.unwrap_or("").into(),
                body: cmd.body.unwrap_or("").into(),
                closed: cmd.closed.unwrap_or(false),
                updated_at: Timestamp::from_utc(Utc::now()),
                assignees: vec![],
                labels: vec![],
            })
        }

        async fn fetch_remote(
            &self,
            _: &str,
            _: &str,
        ) -> PortResult<RemoteTaskSnapshot> {
            self.fetch_returns
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| PortError::NotFound("no fetch fixture".into()))
        }
    }

    async fn setup() -> (SyncService, Arc<InMemoryTaskRepository>, Task, Arc<FakeProvider>) {
        let tasks = Arc::new(InMemoryTaskRepository::new());
        let bindings = Arc::new(InMemoryRepoBindingRepository::new());
        let provider = Arc::new(FakeProvider::default());

        let workspace_id = WorkspaceId::new();
        let binding =
            RepoBinding::new(workspace_id, "git@github.com:o/r.git".into(), "github.com/o/r".into())
                .unwrap();
        let repo_id = binding.id;
        bindings.save(&binding).await.unwrap();

        let task = Task::new_draft(workspace_id, Some(repo_id), "ship it".into()).unwrap();
        tasks.save(&task).await.unwrap();

        let svc = SyncService::new(tasks.clone(), bindings, provider.clone());
        (svc, tasks, task, provider)
    }

    #[tokio::test]
    async fn promote_creates_remote_and_marks_pushed() {
        let (svc, _tasks, task, provider) = setup().await;
        let s = svc.promote(&task.id.to_string()).await.unwrap();
        assert_eq!(s.previous_state, "draft");
        assert_eq!(s.new_state, "pushed");
        assert_eq!(s.remote.as_ref().unwrap().provider, "github");
        assert_eq!(s.remote.as_ref().unwrap().remote_id, "100");
        assert_eq!(
            provider.last_create.lock().unwrap().as_deref(),
            Some("ship it")
        );
    }

    #[tokio::test]
    async fn promote_requires_repo_binding() {
        let (svc, tasks, _, _) = setup().await;
        let mut t = Task::new_draft(WorkspaceId::new(), None, "rogue".into()).unwrap();
        t.repo_id = None;
        tasks.save(&t).await.unwrap();
        let err = svc.promote(&t.id.to_string()).await.unwrap_err();
        assert!(matches!(err, SyncError::NoRepo));
    }

    #[tokio::test]
    async fn push_after_local_edit_marks_synced() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        let mut t = tasks.get(task.id).await.unwrap();
        t.mark_dirty_local().unwrap();
        t.set_body("revised".into());
        tasks.save(&t).await.unwrap();

        let s = svc.push(&task.id.to_string()).await.unwrap();
        assert_eq!(s.previous_state, "dirty_local");
        assert_eq!(s.new_state, "synced");
        let (rid, body) = provider.last_update.lock().unwrap().clone().unwrap();
        assert_eq!(rid, "100");
        assert_eq!(body.as_deref(), Some("revised"));
    }

    #[tokio::test]
    async fn pull_applies_remote_snapshot_when_newer() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();

        // Remote has a newer updated_at and a different title.
        let later = Timestamp::from_utc(Utc::now() + chrono::Duration::seconds(60));
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            title: "new title".into(),
            body: "remote body".into(),
            closed: false,
            updated_at: later,
            assignees: vec!["bob".into()],
            labels: vec![],
        });

        let s = svc.pull(&task.id.to_string()).await.unwrap();
        assert_eq!(s.decision, "pull_remote");
        let after = tasks.get(task.id).await.unwrap();
        assert_eq!(after.title, "new title");
        assert_eq!(after.body, "remote body");
        assert_eq!(after.assignees, vec!["bob".to_string()]);
        assert_eq!(after.state, TaskState::Synced);
    }

    #[tokio::test]
    async fn pull_noop_when_remote_unchanged() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        // Mark the local as synced so the decision will be Noop unless remote is newer.
        let mut t = tasks.get(task.id).await.unwrap();
        t.mark_synced().unwrap();
        tasks.save(&t).await.unwrap();

        let before = tasks.get(task.id).await.unwrap();
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            title: before.title.clone(),
            body: before.body.clone(),
            closed: false,
            updated_at: Timestamp::from_utc(
                before.updated_at.into_inner() - chrono::Duration::seconds(10),
            ),
            assignees: before.assignees.clone(),
            labels: vec![],
        });
        let s = svc.pull(&task.id.to_string()).await.unwrap();
        assert_eq!(s.decision, "noop");
    }
}
