//! repo-link-daemon — background reconciliation + sync loop.
//!
//! One periodic tick performs, for each non-archived workspace:
//! 1. `RepoBindingService::reconcile_worktrees` — fold any vanished
//!    worktrees into the binding status (optionally prune).
//! 2. If a `SyncService` is configured, push every task that is in
//!    `DirtyLocal` state. (Pull-side reconciliation is opt-in to keep the
//!    daemon from hammering the GitHub API; trigger it via `rl sync pull`.)
//!
//! The runtime is `tokio` with a single ticker + a ctrl-c watcher. The loop
//! is fully testable via `Daemon::tick_once`.

use std::sync::Arc;
use std::time::Duration;

use application_sync::SyncService;
use application_workspace::{RepoBindingService, WorkspaceService};
use domain_task::TaskState;
use ports::{FilesystemProbe, TaskFilter, TaskRepository};
use thiserror::Error;
use tokio::signal;

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error(transparent)]
    Port(#[from] ports::PortError),
    #[error("workspace service: {0}")]
    Workspace(String),
    #[error("binding service: {0}")]
    Binding(String),
    #[error("sync: {0}")]
    Sync(String),
}

pub struct Daemon {
    workspaces: WorkspaceService,
    bindings: RepoBindingService,
    tasks: Arc<dyn TaskRepository>,
    probe: Arc<dyn FilesystemProbe>,
    sync: Option<SyncService>,
    prune: bool,
}

impl Daemon {
    pub fn new(
        workspaces: WorkspaceService,
        bindings: RepoBindingService,
        tasks: Arc<dyn TaskRepository>,
        probe: Arc<dyn FilesystemProbe>,
        sync: Option<SyncService>,
    ) -> Self {
        Self {
            workspaces,
            bindings,
            tasks,
            probe,
            sync,
            prune: false,
        }
    }

    /// When true, reconcile passes also drop entries marked `MissingPath`.
    pub fn with_prune(mut self, prune: bool) -> Self {
        self.prune = prune;
        self
    }

    /// One full reconcile + push pass. Returns counts so callers can log
    /// progress and tests can assert.
    pub async fn tick_once(&self) -> Result<TickReport, DaemonError> {
        let mut report = TickReport::default();

        let workspaces = self
            .workspaces
            .list(dto_shared::ListWorkspacesQuery::default())
            .await
            .map_err(|e| DaemonError::Workspace(e.to_string()))?;

        for ws in &workspaces {
            report.workspaces += 1;
            let summary = self
                .bindings
                .reconcile_worktrees(&ws.id, self.probe.as_ref(), self.prune)
                .await
                .map_err(|e| DaemonError::Binding(e.to_string()))?;
            report.repos_checked += summary.repos_checked;
            report.worktrees_checked += summary.worktrees_checked;
            report.marked_missing += summary.marked_missing;
            report.pruned += summary.pruned;

            if let Some(sync) = &self.sync {
                let id: domain_core::WorkspaceId = ws
                    .id
                    .parse()
                    .map_err(|e: domain_core::IdParseError| DaemonError::Sync(e.to_string()))?;
                let dirty = self
                    .tasks
                    .list(TaskFilter {
                        workspace_id: Some(id),
                        state: Some(TaskState::DirtyLocal),
                        ..TaskFilter::default()
                    })
                    .await?;
                for t in dirty {
                    match sync.push(&t.id.to_string()).await {
                        Ok(_) => report.pushed += 1,
                        Err(e) => report.push_failures.push(format!("{}: {e}", t.id)),
                    }
                }
            }
        }

        Ok(report)
    }

    /// Drive `tick_once` on a fixed interval until ctrl-c.
    pub async fn run(self, interval: Duration) -> Result<(), DaemonError> {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Tick once immediately so the daemon is useful at startup, not
        // `interval` seconds later.
        ticker.tick().await;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    match self.tick_once().await {
                        Ok(report) => eprintln!("[daemon] tick: {report:?}"),
                        Err(e) => eprintln!("[daemon] tick error: {e}"),
                    }
                }
                _ = signal::ctrl_c() => {
                    eprintln!("[daemon] shutting down");
                    return Ok(());
                }
            }
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct TickReport {
    pub workspaces: usize,
    pub repos_checked: usize,
    pub worktrees_checked: usize,
    pub marked_missing: usize,
    pub pruned: usize,
    pub pushed: usize,
    pub push_failures: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use domain_repo::RepoBinding;
    use domain_task::Task;
    use domain_workspace::{Workspace, WorkspaceName};
    use ports::{
        PortResult, RemoteTaskCreate, RemoteTaskProvider, RemoteTaskSnapshot, RemoteTaskUpdate,
        RepoBindingRepository, TaskRepository, WorkspaceRepository,
    };
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use testing_fixtures::{
        InMemoryRepoBindingRepository, InMemoryTaskRepository, InMemoryWorkspaceRepository,
        StubFilesystemProbe,
    };

    #[derive(Default)]
    struct CountingProvider {
        creates: AtomicUsize,
        updates: AtomicUsize,
        last_remote_id: Mutex<Option<String>>,
    }

    #[async_trait]
    impl RemoteTaskProvider for CountingProvider {
        async fn create_remote(
            &self,
            cmd: RemoteTaskCreate<'_>,
        ) -> PortResult<RemoteTaskSnapshot> {
            self.creates.fetch_add(1, Ordering::SeqCst);
            *self.last_remote_id.lock().unwrap() = Some("777".into());
            Ok(RemoteTaskSnapshot {
                remote_id: "777".into(),
                title: cmd.title.into(),
                body: cmd.body.into(),
                closed: false,
                updated_at: domain_core::Timestamp::now(),
                assignees: cmd.assignees.to_vec(),
                labels: cmd.labels.to_vec(),
            })
        }

        async fn update_remote(
            &self,
            cmd: RemoteTaskUpdate<'_>,
        ) -> PortResult<RemoteTaskSnapshot> {
            self.updates.fetch_add(1, Ordering::SeqCst);
            Ok(RemoteTaskSnapshot {
                remote_id: cmd.remote_id.into(),
                title: cmd.title.unwrap_or("").into(),
                body: cmd.body.unwrap_or("").into(),
                closed: cmd.closed.unwrap_or(false),
                updated_at: domain_core::Timestamp::now(),
                assignees: vec![],
                labels: vec![],
            })
        }

        async fn fetch_remote(
            &self,
            _: &str,
            _: &str,
        ) -> PortResult<RemoteTaskSnapshot> {
            Err(ports::PortError::NotFound("no fetch fixture".into()))
        }
    }

    #[tokio::test]
    async fn tick_reconciles_and_pushes_dirty() {
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let bind_repo = Arc::new(InMemoryRepoBindingRepository::new());
        let task_repo = Arc::new(InMemoryTaskRepository::new());
        let provider = Arc::new(CountingProvider::default());

        // Seed a workspace + repo binding with a missing worktree + a dirty task.
        let ws = Workspace::new(WorkspaceName::new("scratch").unwrap(), None, true);
        ws_repo.save(&ws).await.unwrap();

        let mut binding = RepoBinding::new(
            ws.id,
            "git@github.com:o/r.git".into(),
            "github.com/o/r".into(),
        )
        .unwrap();
        binding.link_worktree(std::path::PathBuf::from("/tmp/exists"), None);
        binding.link_worktree(std::path::PathBuf::from("/tmp/gone"), None);
        bind_repo.save(&binding).await.unwrap();

        // Probe sees only /tmp/exists.
        let probe = Arc::new(StubFilesystemProbe::new().with_path("/tmp/exists"));

        // A task that was synced + then locally edited.
        let mut task = Task::new_draft(ws.id, Some(binding.id), "edit me".into()).unwrap();
        task.stage_for_sync().unwrap();
        task.promote_to_remote(domain_task::RemoteRef {
            provider: "github".into(),
            remote_id: "777".into(),
        })
        .unwrap();
        task.mark_synced().unwrap();
        task.mark_dirty_local().unwrap();
        task.set_body("new body".into());
        task_repo.save(&task).await.unwrap();

        let workspaces = WorkspaceService::new(ws_repo.clone());
        let bindings = RepoBindingService::new(ws_repo.clone(), bind_repo.clone());
        let provider_dyn: Arc<dyn RemoteTaskProvider> = provider.clone();
        let sync = SyncService::new(task_repo.clone(), bind_repo.clone(), provider_dyn);
        let daemon = Daemon::new(
            workspaces,
            bindings,
            task_repo.clone(),
            probe.clone(),
            Some(sync),
        );

        let report = daemon.tick_once().await.unwrap();
        assert_eq!(report.workspaces, 1);
        assert_eq!(report.repos_checked, 1);
        assert_eq!(report.worktrees_checked, 2);
        assert_eq!(report.marked_missing, 1);
        assert_eq!(report.pruned, 0);
        assert_eq!(report.pushed, 1);
        assert!(report.push_failures.is_empty());
        assert_eq!(provider.updates.load(Ordering::SeqCst), 1);

        // Second tick: nothing dirty, nothing newly missing.
        let report = daemon.tick_once().await.unwrap();
        assert_eq!(report.marked_missing, 0);
        assert_eq!(report.pushed, 0);
    }

    #[tokio::test]
    async fn tick_without_sync_only_reconciles() {
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let bind_repo = Arc::new(InMemoryRepoBindingRepository::new());
        let task_repo = Arc::new(InMemoryTaskRepository::new());

        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        ws_repo.save(&ws).await.unwrap();

        let probe = Arc::new(StubFilesystemProbe::new());
        let workspaces = WorkspaceService::new(ws_repo.clone());
        let bindings = RepoBindingService::new(ws_repo, bind_repo);
        let daemon = Daemon::new(workspaces, bindings, task_repo, probe, None);

        let report = daemon.tick_once().await.unwrap();
        assert_eq!(report.workspaces, 1);
        assert_eq!(report.pushed, 0);
    }
}
