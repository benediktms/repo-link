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

        // Two independent push axes: the title/body/status snapshot (gated on
        // DirtyLocal|Staged) and pending outbound comments (a separate axis —
        // they never dirty the task, so a Synced task may still owe comments).
        let snapshot_dirty = matches!(task.sync, SyncState::DirtyLocal | SyncState::Staged);
        let has_pending_comments = task.comments.iter().any(|c| c.remote_id.is_none());
        if !snapshot_dirty && !has_pending_comments {
            return Err(SyncError::Domain(domain_core::DomainError::transition(
                format!("cannot push from sync={:?} with no pending comments", task.sync),
            )));
        }

        if snapshot_dirty {
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
        }

        // Drain pending comments after the snapshot push (independent of it):
        // POST each, then promote them all to synced in one repo write.
        //
        // Not idempotent across a mid-batch failure: if a later POST fails, the
        // earlier comments are already on GitHub but their local rows stay
        // pending, so a re-run re-POSTs them (duplicate remote comments). GitHub
        // issue comments have no idempotency key, so this at-most-once-per-retry
        // duplication is an accepted tradeoff for a low-frequency operation —
        // never lost comments, never a corrupted sync state.
        if has_pending_comments {
            let mut drained_local_ids = Vec::new();
            let mut pushed = Vec::new();
            for comment in task.comments.iter().filter(|c| c.remote_id.is_none()) {
                // Pending comments loaded from storage carry a surrogate id;
                // skip any in-memory entries that don't (not safely drainable).
                let Some(local_id) = comment.local_id.clone() else {
                    continue;
                };
                pushed.push(
                    self.provider
                        .create_comment(&canonical, &remote.remote_id, &comment.body)
                        .await?,
                );
                drained_local_ids.push(local_id);
            }
            self.tasks
                .mark_comments_pushed(id, &drained_local_ids, &pushed)
                .await?;
        }

        let decision = if snapshot_dirty {
            SyncDecision::PushLocal
        } else {
            SyncDecision::Noop
        };
        Ok(summary(&task, prev, decision))
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

        // A conflict still records the conflicted state below, but we defer the
        // error so comment mirroring (orthogonal to title/body drift) still runs.
        let mut manual_merge: Option<String> = None;
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
                manual_merge = Some(task_id.to_string());
            }
        }

        // Mirror comments regardless of the snapshot decision (even on Noop or a
        // manual-merge conflict): comment activity is orthogonal to title/body
        // drift, and `replace_comments` writes no snapshot, so this can't cause
        // the cosmetic-refresh churn the field-level pull guards against.
        let comments = self
            .provider
            .fetch_comments(&canonical, &remote.remote_id)
            .await?;
        self.tasks.replace_comments(id, &comments).await?;

        if let Some(tid) = manual_merge {
            return Err(SyncError::ManualMerge(tid));
        }

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
    use ports::{PortResult, RemoteComment, RemoteTaskSnapshot};
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
        comments: Mutex<Vec<RemoteComment>>,
        created_comments: Mutex<Vec<String>>,
    }

    impl FakeProvider {
        fn set_fetch(&self, snap: RemoteTaskSnapshot) {
            *self.fetch_returns.lock().unwrap() = Some(snap);
        }

        fn set_comments(&self, comments: Vec<RemoteComment>) {
            *self.comments.lock().unwrap() = comments;
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

        async fn fetch_comments(&self, _: &str, _: &str) -> PortResult<Vec<RemoteComment>> {
            Ok(self.comments.lock().unwrap().clone())
        }

        async fn create_comment(
            &self,
            _: &str,
            _: &str,
            body: &str,
        ) -> PortResult<RemoteComment> {
            let mut created = self.created_comments.lock().unwrap();
            created.push(body.to_string());
            Ok(RemoteComment {
                remote_id: format!("c{}", created.len()),
                author: "remote-bot".into(),
                body: body.to_string(),
                created_at: Timestamp::from_utc(Utc::now()),
            })
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

    #[tokio::test]
    async fn pull_mirrors_comments_even_on_manual_merge_conflict() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();

        // Local edit → DirtyLocal, and a newer remote → remote_dirty. Under the
        // default ManualMerge policy this resolves to RequireManualMerge.
        let mut t = tasks.get(task.id).await.unwrap();
        t.mark_dirty_local().unwrap();
        t.set_body("local edit".into());
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        let later = Timestamp::from_utc(Utc::now() + chrono::Duration::seconds(60));
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            title: "remote title".into(),
            body: "remote body".into(),
            closed: false,
            updated_at: later,
            assignees: vec![],
            labels: vec![],
        });
        provider.set_comments(vec![RemoteComment {
            remote_id: "7".into(),
            author: "octocat".into(),
            body: "ping".into(),
            created_at: Timestamp::from_utc(Utc::now()),
        }]);

        let err = svc.pull(&task.id.to_string()).await.unwrap_err();
        assert!(matches!(err, SyncError::ManualMerge(_)));

        // The conflict still surfaces an error, but comments are mirrored anyway.
        let after = tasks.get(task.id).await.unwrap();
        assert_eq!(after.sync, SyncState::Conflict);
        assert_eq!(after.comments.len(), 1);
        assert_eq!(after.comments[0].body, "ping");
    }

    #[tokio::test]
    async fn push_drains_pending_comments_on_synced_task() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        assert_eq!(tasks.get(task.id).await.unwrap().sync, SyncState::Synced);

        // A pending comment must NOT have dirtied the task.
        tasks
            .add_pending_comment(task.id, "me", "hello world", Timestamp::now())
            .await
            .unwrap();
        assert_eq!(tasks.get(task.id).await.unwrap().sync, SyncState::Synced);

        let s = svc.push(&task.id.to_string()).await.unwrap();
        // Comment-only push: the snapshot axis is a noop, task stays synced.
        assert_eq!(s.decision, "noop");
        assert_eq!(s.new_state, "synced");

        // create_comment was called; update_remote (title/body) was NOT.
        assert_eq!(*provider.created_comments.lock().unwrap(), vec!["hello world".to_string()]);
        assert!(provider.last_update.lock().unwrap().is_none());

        // The pending comment is now synced.
        let after = tasks.get(task.id).await.unwrap();
        assert_eq!(after.comments.len(), 1);
        assert!(after.comments[0].remote_id.is_some());
    }

    #[tokio::test]
    async fn push_drains_comments_and_snapshot_when_dirty() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();

        let mut t = tasks.get(task.id).await.unwrap();
        t.mark_dirty_local().unwrap();
        t.set_body("revised".into());
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();
        tasks
            .add_pending_comment(task.id, "me", "also a comment", Timestamp::now())
            .await
            .unwrap();

        let s = svc.push(&task.id.to_string()).await.unwrap();
        assert_eq!(s.decision, "push_local");
        assert_eq!(s.new_state, "synced");

        // Both axes pushed.
        let recorded = provider.last_update.lock().unwrap().clone().unwrap();
        assert_eq!(recorded.body.as_deref(), Some("revised"));
        assert_eq!(*provider.created_comments.lock().unwrap(), vec!["also a comment".to_string()]);

        let after = tasks.get(task.id).await.unwrap();
        assert!(after.comments.iter().all(|c| c.remote_id.is_some()));
    }

    #[tokio::test]
    async fn push_errors_when_clean_and_no_pending_comments() {
        let (svc, tasks, task, _provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        assert_eq!(tasks.get(task.id).await.unwrap().sync, SyncState::Synced);

        let err = svc.push(&task.id.to_string()).await.unwrap_err();
        assert!(matches!(err, SyncError::Domain(_)));
    }
}
