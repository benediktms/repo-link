//! application-sync — orchestrates remote promotion / push / pull.
//!
//! Local SQLite is authoritative for draft state; once a task has been
//! pushed, GitHub becomes the source of truth. Sync transitions follow
//! [`SyncState`]; lifecycle ([`TaskStatus`]) is orthogonal and only
//! consulted to skip Archived tasks.

use std::sync::Arc;

use domain_core::{IdParseError, TaskId};
use domain_sync::{SyncDecision, SyncPolicy, decide};
use domain_task::{RemoteRef, SnapshotSource, SyncState, Task, TaskStatus};
use dto_shared::{RemoteRefDto, SyncSummaryDto};
use ports::{
    PortError, RemoteStateReason, RemoteTaskCreate, RemoteTaskProvider, RemoteTaskUpdate,
    RepoBindingRepository, TaskRepository,
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
    #[error("task is archived; unarchive before syncing")]
    Archived,
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

    /// Stage (if needed) and promote a `LocalOnly`/`Staged` task to a remote
    /// issue. `previous_state` / `new_state` in the summary describe the
    /// **sync** state — lifecycle stays untouched.
    pub async fn promote(&self, task_id: &str) -> Result<SyncSummaryDto> {
        let id: TaskId = task_id.parse()?;
        let mut task = self.tasks.get(id).await?;
        ensure_not_archived(&task)?;
        let canonical = self.canonical_for(&task).await?;
        let prev = task.sync;

        if task.sync == SyncState::LocalOnly {
            task.stage_for_sync()?;
        }
        if task.sync != SyncState::Staged {
            return Err(SyncError::Domain(domain_core::DomainError::transition(
                format!("cannot promote from sync={:?}", task.sync),
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
        self.tasks.save(&task, SnapshotSource::Promote).await?;
        Ok(summary(&task, prev, SyncDecision::PushLocal))
    }

    // TODO(online-sync-mode): the current model is "edit locally, daemon
    // pushes on its next tick" — non-blocking and offline-friendly. A
    // future opt-in mode would have CLI mutations fire the remote update
    // inline when sync is Synced + the network is reachable, so changes
    // round-trip in real time. Trade-off: every CLI command would block
    // on a GitHub round-trip (~200-800ms typical) and rate limits become
    // a concern with batch commands. Default stays offline-first; this
    // would land as `--online` flag or `RepoLinkConfig::online_mode: bool`.
    /// Push local edits (`sync = DirtyLocal` or `Staged`) to the remote.
    pub async fn push(&self, task_id: &str) -> Result<SyncSummaryDto> {
        let id: TaskId = task_id.parse()?;
        let mut task = self.tasks.get(id).await?;
        let canonical = self.canonical_for(&task).await?;
        let prev = task.sync;
        let remote = task.remote.as_ref().ok_or(SyncError::NoRemote)?.clone();

        if !matches!(task.sync, SyncState::DirtyLocal | SyncState::Staged) {
            return Err(SyncError::Domain(domain_core::DomainError::transition(
                format!("cannot push from sync={:?}", task.sync),
            )));
        }

        // Mirror the lifecycle status onto the remote issue's open/closed
        // bit + state_reason. `Done` closes as `Completed`; `Archived`
        // closes as `NotPlanned`. Any open status reopens (we don't
        // currently know whether the remote was previously closed; sending
        // `Reopened` unconditionally is harmless on GitHub when state is
        // already open and informative otherwise).
        let (closed, state_reason) = match task.status {
            TaskStatus::Done => (true, Some(RemoteStateReason::Completed)),
            TaskStatus::Archived => (true, Some(RemoteStateReason::NotPlanned)),
            TaskStatus::Open | TaskStatus::InProgress | TaskStatus::Blocked => {
                (false, Some(RemoteStateReason::Reopened))
            }
        };
        self.provider
            .update_remote(RemoteTaskUpdate {
                canonical_repo: &canonical,
                remote_id: &remote.remote_id,
                title: Some(&task.title),
                body: Some(&task.body),
                closed: Some(closed),
                state_reason,
            })
            .await?;

        task.confirm_synced(SnapshotSource::Push)?;
        self.tasks.save(&task, SnapshotSource::Push).await?;
        Ok(summary(&task, prev, SyncDecision::PushLocal))
    }

    /// Pull the latest remote snapshot and reconcile.
    pub async fn pull(&self, task_id: &str) -> Result<SyncSummaryDto> {
        let id: TaskId = task_id.parse()?;
        let mut task = self.tasks.get(id).await?;
        ensure_not_archived(&task)?;
        let canonical = self.canonical_for(&task).await?;
        let remote = task.remote.as_ref().ok_or(SyncError::NoRemote)?.clone();
        let prev = task.sync;

        let snap = self
            .provider
            .fetch_remote(&canonical, &remote.remote_id)
            .await?;
        let remote_changed = snap.updated_at.into_inner() > task.updated_at.into_inner();
        let decision = decide(task.sync, remote_changed, self.policy);

        match decision {
            SyncDecision::Noop => {}
            SyncDecision::PullRemote => {
                // Capture local state *before* remote overwrites it — this is the
                // undo target if the user wants to revert the pull.
                self.tasks.save(&task, SnapshotSource::PrePull).await?;
                // Transition to DirtyRemote so confirm_synced accepts the Pull
                // source (it requires Staged | DirtyLocal | DirtyRemote).
                task.mark_dirty_remote()?;
                // Direct field assignment (bypassing setter helpers that would
                // re-trigger dirty detection against the OLD baseline). Status is
                // intentionally NOT overwritten — GitHub's open/closed doesn't
                // map onto our 5-state lifecycle cleanly.
                task.title = snap.title.clone();
                task.body = snap.body.clone();
                task.assignees = snap.assignees.clone();
                task.confirm_synced(SnapshotSource::Pull)?;
                self.tasks.save(&task, SnapshotSource::Pull).await?;
            }
            SyncDecision::PushLocal => {
                // TODO(rwr/push-on-pull): a PushLocal decision returned from
                // pull means the local side is ahead. Today the user has to
                // call `sync push` explicitly to flush it; we could fold
                // that into pull when we want a one-shot reconcile.
            }
            SyncDecision::RequireManualMerge => {
                task.mark_conflicted()?;
                self.tasks.save(&task, SnapshotSource::LocalEdit).await?;
                return Err(SyncError::ManualMerge(task_id.to_string()));
            }
        }

        // Mirror comments regardless of the snapshot decision (even on Noop):
        // comment activity is orthogonal to title/body drift, and
        // `replace_comments` writes no snapshot, so this can't cause the
        // cosmetic-refresh churn the field-level pull guards against.
        let comments = self
            .provider
            .fetch_comments(&canonical, &remote.remote_id)
            .await?;
        self.tasks.replace_comments(id, &comments).await?;

        Ok(summary(&task, prev, decision))
    }

    async fn canonical_for(&self, task: &Task) -> Result<String> {
        let repo_id = task.repo_id.ok_or(SyncError::NoRepo)?;
        let binding = self.bindings.get(repo_id).await?;
        Ok(binding.canonical_url)
    }
}

fn ensure_not_archived(task: &Task) -> Result<()> {
    if task.status == TaskStatus::Archived {
        Err(SyncError::Archived)
    } else {
        Ok(())
    }
}

fn summary(task: &Task, prev: SyncState, decision: SyncDecision) -> SyncSummaryDto {
    SyncSummaryDto {
        task_id: task.id.to_string(),
        previous_state: enum_str(&prev),
        new_state: enum_str(&task.sync),
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

    #[derive(Clone)]
    struct RecordedUpdate {
        remote_id: String,
        body: Option<String>,
        closed: Option<bool>,
        state_reason: Option<RemoteStateReason>,
    }

    #[derive(Default)]
    struct FakeProvider {
        last_create: Mutex<Option<String>>,
        last_update: Mutex<Option<RecordedUpdate>>,
        fetch_returns: Mutex<Option<RemoteTaskSnapshot>>,
    }

    impl FakeProvider {
        fn set_fetch(&self, snap: RemoteTaskSnapshot) {
            *self.fetch_returns.lock().unwrap() = Some(snap);
        }
    }

    #[async_trait]
    impl RemoteTaskProvider for FakeProvider {
        async fn create_remote(&self, cmd: RemoteTaskCreate<'_>) -> PortResult<RemoteTaskSnapshot> {
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

        async fn update_remote(&self, cmd: RemoteTaskUpdate<'_>) -> PortResult<RemoteTaskSnapshot> {
            *self.last_update.lock().unwrap() = Some(RecordedUpdate {
                remote_id: cmd.remote_id.into(),
                body: cmd.body.map(str::to_owned),
                closed: cmd.closed,
                state_reason: cmd.state_reason,
            });
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

        async fn fetch_remote(&self, _: &str, _: &str) -> PortResult<RemoteTaskSnapshot> {
            self.fetch_returns
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| PortError::NotFound("no fetch fixture".into()))
        }
    }

    async fn setup() -> (
        SyncService,
        Arc<InMemoryTaskRepository>,
        Task,
        Arc<FakeProvider>,
    ) {
        let tasks = Arc::new(InMemoryTaskRepository::new());
        let bindings = Arc::new(InMemoryRepoBindingRepository::new());
        let provider = Arc::new(FakeProvider::default());

        let workspace_id = WorkspaceId::new();
        let binding = RepoBinding::new(
            workspace_id,
            "git@github.com:o/r.git".into(),
            "github.com/o/r".into(),
        )
        .unwrap();
        let repo_id = binding.id;
        bindings.save(&binding).await.unwrap();

        let task = Task::new_draft(workspace_id, Some(repo_id), "ship it".into()).unwrap();
        tasks.save(&task, SnapshotSource::LocalEdit).await.unwrap();

        let svc = SyncService::new(tasks.clone(), bindings, provider.clone());
        (svc, tasks, task, provider)
    }

    #[tokio::test]
    async fn promote_creates_remote_and_marks_pushed() {
        let (svc, _tasks, task, provider) = setup().await;
        let s = svc.promote(&task.id.to_string()).await.unwrap();
        assert_eq!(s.previous_state, "local_only");
        assert_eq!(s.new_state, "synced");
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
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();
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
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        let s = svc.push(&task.id.to_string()).await.unwrap();
        assert_eq!(s.previous_state, "dirty_local");
        assert_eq!(s.new_state, "synced");
        let recorded = provider.last_update.lock().unwrap().clone().unwrap();
        assert_eq!(recorded.remote_id, "100");
        assert_eq!(recorded.body.as_deref(), Some("revised"));
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
        assert_eq!(after.sync, SyncState::Synced);
    }

    #[tokio::test]
    async fn push_archived_task_closes_remote_with_not_planned() {
        let (svc, tasks, task, provider) = setup().await;
        // Promote → Synced, then archive → DirtyLocal.
        svc.promote(&task.id.to_string()).await.unwrap();
        let mut t = tasks.get(task.id).await.unwrap();
        t.archive().unwrap();
        // archive() + reconcile_dirty_against_baseline transitions Synced → DirtyLocal.
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        let s = svc.push(&task.id.to_string()).await.unwrap();
        assert_eq!(s.new_state, "synced");

        let recorded = provider.last_update.lock().unwrap().clone().unwrap();
        assert_eq!(recorded.remote_id, "100");
        assert_eq!(recorded.closed, Some(true));
        assert!(matches!(
            recorded.state_reason,
            Some(RemoteStateReason::NotPlanned)
        ));
    }

    #[tokio::test]
    async fn pull_noop_when_remote_unchanged() {
        let (svc, tasks, task, provider) = setup().await;
        // promote lands directly on Synced now (sync state transition is
        // collapsed into the promotion), so no extra mark_synced needed.
        svc.promote(&task.id.to_string()).await.unwrap();

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
