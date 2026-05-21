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

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use application_sync::SyncService;
use application_workspace::{RepoBindingService, WorkspaceService};
use clap::Parser;
use domain_task::SyncState;
use infra_config::RepoLinkConfig;
use infra_filesystem::TokioFilesystemProbe;
use infra_github::GithubTaskProvider;
use infra_sqlite::{
    SqliteRepoBindingRepository, SqliteTaskRepository, SqliteWorkspaceRepository, open_from_path,
};
use ports::{FilesystemProbe, TaskFilter, TaskRepository};
use thiserror::Error;
use tokio::signal;
use tracing::{Instrument, error, info, info_span, warn};

mod logging;
pub use logging::{LogFormat, init_subscriber};

#[derive(Parser, Debug)]
#[command(name = "repo-link-daemon", version, about = "Background reconciler for repo-link")]
pub struct Args {
    /// Tick interval in seconds.
    #[arg(long, default_value_t = 60, env = "REPO_LINK_INTERVAL_SECS")]
    pub interval_secs: u64,

    /// When set, the daemon prunes worktree entries it just marked missing.
    #[arg(long)]
    pub prune: bool,

    /// Database path override (defaults to platform data dir).
    #[arg(long, env = "REPO_LINK_DB")]
    pub db: Option<PathBuf>,

    /// Log output format. `pretty` writes ANSI-coloured text to stdout
    /// for foreground/dev runs; `json` writes a daily-rotated
    /// `daemon.log` next to the database for use under launchd/systemd.
    #[arg(long, value_enum, default_value_t = LogFormat::Pretty)]
    pub log_format: LogFormat,
}

/// Library entrypoint shared by both `repo-link-daemon` and `rld` bin shims.
pub async fn run_cli() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut cfg = RepoLinkConfig::from_env()?;
    if let Some(db) = args.db {
        cfg = cfg.with_database_path(db);
    }
    if let Some(parent) = cfg.database_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    // Logs live next to the SQLite db so all daemon state co-locates under
    // the platform data dir. The guard keeps the non-blocking file-write
    // worker alive; dropping it at process exit flushes any buffered events.
    let log_dir = cfg
        .database_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let _log_guard = init_subscriber(args.log_format, &log_dir);

    let db = open_from_path(&cfg.database_path).await?;
    let workspaces_repo: Arc<dyn ports::WorkspaceRepository> =
        Arc::new(SqliteWorkspaceRepository::new(db.clone()));
    let bindings_repo: Arc<dyn ports::RepoBindingRepository> =
        Arc::new(SqliteRepoBindingRepository::new(db.clone()));
    let tasks_repo: Arc<dyn ports::TaskRepository> = Arc::new(SqliteTaskRepository::new(db));

    let workspaces = WorkspaceService::new(workspaces_repo.clone());
    let bindings = RepoBindingService::new(workspaces_repo, bindings_repo.clone());

    let probe: Arc<dyn ports::FilesystemProbe> = Arc::new(TokioFilesystemProbe::new());

    let sync = cfg.github_token.clone().map(|token| {
        let provider: Arc<dyn ports::RemoteTaskProvider> =
            Arc::new(GithubTaskProvider::new(token));
        SyncService::new(tasks_repo.clone(), bindings_repo, provider)
    });

    let daemon = Daemon::new(workspaces, bindings, tasks_repo, probe, sync).with_prune(args.prune);
    info!(
        interval_secs = args.interval_secs,
        prune = args.prune,
        db = %cfg.database_path.display(),
        log_format = ?args.log_format,
        "daemon starting"
    );
    daemon.run(Duration::from_secs(args.interval_secs)).await?;
    Ok(())
}

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
            // Per-workspace span so subsequent reconcile/push events nest
            // under it for json-grep-by-trace-id and `RUST_LOG=…` filtering.
            async {
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
                info!(
                    repos_checked = summary.repos_checked,
                    worktrees_checked = summary.worktrees_checked,
                    marked_missing = summary.marked_missing,
                    pruned = summary.pruned,
                    "reconcile complete"
                );

                if let Some(sync) = &self.sync {
                    let id: domain_core::WorkspaceId = ws
                        .id
                        .parse()
                        .map_err(|e: domain_core::IdParseError| DaemonError::Sync(e.to_string()))?;
                    let dirty = self
                        .tasks
                        .list(TaskFilter {
                            workspace_id: Some(id),
                            sync_state: Some(SyncState::DirtyLocal),
                            ..TaskFilter::default()
                        })
                        .await?;
                    for t in dirty {
                        match sync.push(&t.id.to_string()).await {
                            Ok(_) => {
                                report.pushed += 1;
                                info!(task_id = %t.id, "pushed dirty task");
                            }
                            Err(e) => {
                                let msg = format!("{}: {e}", t.id);
                                warn!(task_id = %t.id, error = %e, "push failed");
                                report.push_failures.push(msg);
                            }
                        }
                    }
                }
                Ok::<(), DaemonError>(())
            }
            .instrument(info_span!("workspace_tick", workspace_id = %ws.id, name = %ws.name))
            .await?;
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
                        Ok(report) => info!(
                            workspaces = report.workspaces,
                            repos_checked = report.repos_checked,
                            worktrees_checked = report.worktrees_checked,
                            marked_missing = report.marked_missing,
                            pruned = report.pruned,
                            pushed = report.pushed,
                            push_failures = report.push_failures.len(),
                            "tick complete"
                        ),
                        Err(e) => error!(error = %e, "tick failed"),
                    }
                }
                _ = signal::ctrl_c() => {
                    info!("ctrl-c received; shutting down");
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
    use domain_task::{SnapshotSource, Task};
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
        // promote_to_remote already lands on Synced — go straight to DirtyLocal
        // to simulate a post-sync local edit.
        task.mark_dirty_local().unwrap();
        task.set_body("new body".into());
        task_repo.save(&task, SnapshotSource::LocalEdit).await.unwrap();

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
