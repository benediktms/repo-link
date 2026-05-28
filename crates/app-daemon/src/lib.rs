//! rld — repo-link's background reconciliation + sync daemon.
//!
//! One periodic tick performs, for each non-archived workspace:
//! 1. `RepoBindingService::reconcile_worktrees` (mark-only) — fold any
//!    vanished worktrees into the binding status by flipping them to
//!    `MissingPath`. Never prunes from this call.
//! 2. Grace-counter pass (only when `--prune` is set) — re-probe each
//!    `MissingPath` worktree, bump a process-local counter while it stays
//!    missing, and drop the link once the counter hits
//!    `missing_grace_ticks` (default 3) consecutive misses. The counter
//!    resets if the path is observable again, so a transient unmount
//!    won't trigger a prune. Counts are NOT persisted across daemon
//!    restarts — restart resets to zero, which is the safer direction.
//! 3. If a `SyncService` is configured, push every task that is in
//!    `DirtyLocal` state. (Pull-side reconciliation is opt-in to keep the
//!    daemon from hammering the GitHub API; trigger it via `rl sync pull`.)
//!
//! The runtime is `tokio` with a single ticker + a ctrl-c watcher. The loop
//! is fully testable via `Daemon::tick_once`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use application_sync::SyncService;
use application_workspace::{RepoBindingService, WorkspaceService};
use clap::Parser;
use domain_core::RepoId;
use domain_task::SyncState;
use dto_shared::UnlinkWorktreeCmd;
use infra_config::RepoLinkConfig;
use infra_filesystem::TokioFilesystemProbe;
use infra_github::GithubTaskProvider;
use infra_sqlite::{
    SqliteRepoBindingRepository, SqliteTaskRepository, SqliteWorkspaceRepository, open_from_path,
};
use ports::{FilesystemProbe, TaskFilter, TaskRepository};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::signal;
use tracing::{Instrument, error, info, info_span, warn};

mod logging;
pub use logging::{LogFormat, init_subscriber};

/// Serde-serialised form of `domain_repo::LinkStatus::MissingPath` —
/// the enum carries `#[serde(rename_all = "snake_case")]`, so the DTO
/// surfaces it as this literal. Defined once here so a future rename of
/// the enum (or a casing change on the serde rename) doesn't silently
/// disable the grace-prune match. The daemon's unit tests round-trip a
/// real binding through the DTO mapping, so drift between this constant
/// and the actual serialised form fails the test suite on the next run.
const STATUS_MISSING_PATH: &str = "missing_path";

#[derive(Parser, Debug)]
#[command(name = "rld", version, about = "Background reconciler for repo-link")]
pub struct Args {
    /// Tick interval in seconds.
    #[arg(long, default_value_t = 60, env = "REPO_LINK_INTERVAL_SECS")]
    pub interval_secs: u64,

    /// When set, the daemon drops worktree entries whose paths have stayed
    /// missing across `--missing-grace-ticks` consecutive ticks (default 3).
    /// Without this flag, missing paths are marked `MissingPath` on the
    /// binding but never removed.
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

    /// Number of consecutive ticks a worktree path must be missing before
    /// `--prune` actually drops it. Wall-clock grace =
    /// `--interval-secs × --missing-grace-ticks` (defaults: 60s × 3 = 3
    /// minutes). The counter is process-local — restarting the daemon
    /// resets it to zero, so short-lived runs cannot trigger a
    /// grace-protected prune.
    #[arg(long, default_value_t = 3, env = "REPO_LINK_MISSING_GRACE_TICKS")]
    pub missing_grace_ticks: u32,
}

/// Library entrypoint called from the `rld` bin shim.
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

    let sync = match cfg.github_token.clone() {
        Some(token) => {
            let provider: Arc<dyn ports::RemoteTaskProvider> =
                Arc::new(GithubTaskProvider::new(token)?);
            Some(SyncService::new(
                tasks_repo.clone(),
                bindings_repo,
                provider,
            ))
        }
        None => None,
    };

    let daemon = Daemon::new(workspaces, bindings, tasks_repo, probe, sync)
        .with_prune(args.prune)
        .with_missing_grace_ticks(args.missing_grace_ticks)
        // Co-locate last_tick.json with the db + log so a `--db` override
        // relocates the whole daemon state consistently.
        .with_state_dir(log_dir.clone())
        .with_interval_secs(args.interval_secs);
    // Read the coerced value back from the daemon so a run with
    // `REPO_LINK_MISSING_GRACE_TICKS=0` logs the effective `1` rather
    // than the raw input. Single source of truth lives in
    // `with_missing_grace_ticks`.
    info!(
        interval_secs = args.interval_secs,
        prune = args.prune,
        missing_grace_ticks = daemon.missing_grace_ticks(),
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
    missing_grace_ticks: u32,
    // Process-local grace counter. Keyed on (repo binding id, worktree path);
    // increments while a path is observed missing this tick, resets to zero
    // (entry removed) when the path returns. No persistence — daemon restart
    // forgets all counts, which is the safer direction.
    miss_counts: Mutex<HashMap<(RepoId, PathBuf), u32>>,
    // Override for the directory that holds heartbeat state (last_tick.json).
    // `None` resolves via `infra_config::default_last_tick_path()`; tests set
    // this to a tempdir to keep `cargo test` away from the platform data dir.
    state_dir: Option<PathBuf>,
    // Tick cadence in seconds, surfaced in last_tick.json so `daemon status`
    // can decide whether the daemon is wedged (tick_at older than 2 × this).
    interval_secs: u64,
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
            missing_grace_ticks: 3,
            miss_counts: Mutex::new(HashMap::new()),
            state_dir: None,
            interval_secs: 60,
        }
    }

    /// When true, reconcile passes also drop entries marked `MissingPath` —
    /// but only after they've been observed missing for
    /// `missing_grace_ticks` consecutive ticks.
    pub fn with_prune(mut self, prune: bool) -> Self {
        self.prune = prune;
        self
    }

    /// Redirect heartbeat state away from the platform data dir. Production
    /// callers point this at the same parent as the SQLite db (so `--db`
    /// overrides relocate the whole daemon state consistently); tests point
    /// it at a `tempfile::TempDir`.
    pub fn with_state_dir(mut self, dir: PathBuf) -> Self {
        self.state_dir = Some(dir);
        self
    }

    /// Tick cadence in seconds, embedded in `last_tick.json` so `daemon
    /// status` can flag a wedged daemon. Does not change the actual run-loop
    /// cadence — pass the duration to [`Self::run`] for that.
    pub fn with_interval_secs(mut self, n: u64) -> Self {
        self.interval_secs = n;
        self
    }

    /// How many consecutive missed probes must elapse before `--prune`
    /// actually unlinks a `MissingPath` worktree. Values below 1 are coerced
    /// to 1 (legacy "prune on first miss" behaviour). This builder is the
    /// canonical enforcement point for that floor — callers should not
    /// pre-coerce, they should pass the raw CLI/env value through.
    pub fn with_missing_grace_ticks(mut self, n: u32) -> Self {
        self.missing_grace_ticks = n.max(1);
        self
    }

    /// Effective grace threshold after coercion. Exposed so the startup
    /// log (or future diagnostics) can surface the actual behaviour
    /// instead of recomputing `args.missing_grace_ticks.max(1)` at the
    /// call site.
    pub fn missing_grace_ticks(&self) -> u32 {
        self.missing_grace_ticks
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

        // Accumulated across workspaces and used for a single GC pass after
        // the loop. Per-workspace GC is incorrect because `miss_counts` is
        // process-global — retaining only one workspace's keys would drop
        // every other workspace's counter on each tick.
        let mut all_valid_keys: HashSet<(RepoId, PathBuf)> = HashSet::new();

        for ws in &workspaces {
            // Per-workspace span so subsequent reconcile/push events nest
            // under it for json-grep-by-trace-id and `RUST_LOG=…` filtering.
            async {
                report.workspaces += 1;
                // Reconcile always runs in mark-only mode now. Pruning is
                // gated by the grace counter below so transient unmounts
                // don't nuke a worktree on the first missed tick.
                let summary = self
                    .bindings
                    .reconcile_worktrees(&ws.id, self.probe.as_ref(), false)
                    .await
                    .map_err(|e| DaemonError::Binding(e.to_string()))?;
                report.repos_checked += summary.repos_checked;
                report.worktrees_checked += summary.worktrees_checked;
                report.marked_missing += summary.marked_missing;

                let pruned_this_workspace = if self.prune {
                    let (pruned, valid) = self.apply_grace_prune(&ws.id).await?;
                    report.pruned += pruned;
                    all_valid_keys.extend(valid);
                    pruned
                } else {
                    0
                };

                info!(
                    repos_checked = summary.repos_checked,
                    worktrees_checked = summary.worktrees_checked,
                    marked_missing = summary.marked_missing,
                    pruned = pruned_this_workspace,
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

        // Single GC pass across the union of all workspaces' valid-this-tick
        // keys. Drops counter entries whose `(RepoId, PathBuf)` no longer
        // corresponds to a `MissingPath` worktree anywhere (binding deleted,
        // user manually unlinked, etc.). Skipped when `prune` is off because
        // `apply_grace_prune` never ran, so `all_valid_keys` is empty and
        // the retain would wipe the whole map — `miss_counts` is also empty
        // in that case so it's a no-op, but the explicit gate makes the
        // invariant obvious.
        if self.prune {
            self.miss_counts
                .lock()
                .unwrap()
                .retain(|k, _| all_valid_keys.contains(k));
        }

        // Heartbeat: observability, not correctness. A failed write must
        // never abort the tick — the daemon kept its contract; only `daemon
        // status` loses its "wedged?" signal for one tick.
        self.write_last_tick(&report);

        Ok(report)
    }

    fn write_last_tick(&self, report: &TickReport) {
        let path = match &self.state_dir {
            Some(dir) => dir.join("last_tick.json"),
            None => match infra_config::default_last_tick_path() {
                Ok(p) => p,
                Err(e) => {
                    warn!(error = %e, "could not resolve last_tick path");
                    return;
                }
            },
        };
        if let Err(e) = write_last_tick_atomic(&path, self.interval_secs, report) {
            warn!(error = %e, path = %path.display(), "failed to write last_tick.json");
        }
    }

    /// Grace-counter pass for a single workspace. For each binding, re-probe
    /// every worktree currently in `MissingPath` status:
    /// - If the path is still missing this tick, bump the counter. When it
    ///   reaches `missing_grace_ticks`, drop the worktree via
    ///   `unlink_worktree` and forget the counter entry.
    /// - If the path is observable again, drop the counter entry (consecutive
    ///   misses, not cumulative). We deliberately don't transition the link
    ///   status back to `Linked` here — that's the domain's job on an
    ///   explicit re-link, and conflating recovery with the prune janitor
    ///   would expand this phase's scope.
    ///
    /// Returns `(pruned, still_valid)`. `still_valid` is the set of counter
    /// keys this workspace's pass deemed alive this tick — i.e. those whose
    /// path is in `MissingPath` and observably missing. The caller is
    /// responsible for unioning these across all workspaces *before* GC,
    /// because `miss_counts` is process-global: a per-workspace retain would
    /// drop other workspaces' counters every tick. See
    /// `grace_counter_survives_across_multiple_workspaces` test.
    async fn apply_grace_prune(
        &self,
        workspace_id: &str,
    ) -> Result<(usize, HashSet<(RepoId, PathBuf)>), DaemonError> {
        let bindings = self
            .bindings
            .list(workspace_id)
            .await
            .map_err(|e| DaemonError::Binding(e.to_string()))?;

        let mut still_valid: HashSet<(RepoId, PathBuf)> = HashSet::new();
        let mut pruned = 0usize;

        for b in &bindings {
            let repo_id: RepoId =
                b.id.parse()
                    .map_err(|e: domain_core::IdParseError| DaemonError::Binding(e.to_string()))?;

            for wt in &b.worktrees {
                if wt.status != STATUS_MISSING_PATH {
                    continue;
                }
                let path = PathBuf::from(&wt.path);
                let key = (repo_id, path.clone());

                let observed_missing = !self.probe.path_exists(&path).await?;

                let new_count = {
                    let mut counts = self.miss_counts.lock().unwrap();
                    if observed_missing {
                        let entry = counts.entry(key.clone()).or_insert(0);
                        *entry = entry.saturating_add(1);
                        still_valid.insert(key.clone());
                        *entry
                    } else {
                        counts.remove(&key);
                        0
                    }
                };

                if new_count == 0 {
                    continue;
                }
                if new_count >= self.missing_grace_ticks {
                    self.bindings
                        .unlink_worktree(UnlinkWorktreeCmd {
                            repo_id: b.id.clone(),
                            path: wt.path.clone(),
                        })
                        .await
                        .map_err(|e| DaemonError::Binding(e.to_string()))?;
                    self.miss_counts.lock().unwrap().remove(&key);
                    still_valid.remove(&key);
                    pruned += 1;
                } else {
                    warn!(
                        path = %wt.path,
                        consecutive = new_count,
                        threshold = self.missing_grace_ticks,
                        "deferring prune"
                    );
                }
            }
        }

        Ok((pruned, still_valid))
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

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TickReport {
    pub workspaces: usize,
    pub repos_checked: usize,
    pub worktrees_checked: usize,
    pub marked_missing: usize,
    pub pruned: usize,
    pub pushed: usize,
    pub push_failures: Vec<String>,
}

/// Heartbeat artefact written atomically at the end of every `tick_once`.
/// Consumed by `rl daemon status` to surface "running but wedged" — a
/// daemon whose unit is loaded but whose `tick_at` is older than expected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastTick {
    pub tick_at: chrono::DateTime<chrono::Utc>,
    pub interval_secs: u64,
    pub report: TickReport,
}

/// Atomic write: serialise to a temp file in the destination directory,
/// then `rename` over the target. Same-directory rename is atomic on every
/// POSIX filesystem, so readers never see a half-written heartbeat.
fn write_last_tick_atomic(
    path: &std::path::Path,
    interval_secs: u64,
    report: &TickReport,
) -> std::io::Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let last_tick = LastTick {
        tick_at: chrono::Utc::now(),
        interval_secs,
        report: report.clone(),
    };
    let bytes =
        serde_json::to_vec_pretty(&last_tick).map_err(|e| std::io::Error::other(e.to_string()))?;
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(&bytes)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
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
        async fn create_remote(&self, cmd: RemoteTaskCreate<'_>) -> PortResult<RemoteTaskSnapshot> {
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

        async fn update_remote(&self, cmd: RemoteTaskUpdate<'_>) -> PortResult<RemoteTaskSnapshot> {
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

        async fn fetch_remote(&self, _: &str, _: &str) -> PortResult<RemoteTaskSnapshot> {
            Err(ports::PortError::NotFound("no fetch fixture".into()))
        }

        async fn create_comment(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> PortResult<ports::RemoteComment> {
            Err(ports::PortError::NotFound("no comment fixture".into()))
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
        task.promote_to_remote(domain_task::RemoteRef::new("github", "777"))
            .unwrap();
        // promote_to_remote already lands on Synced — go straight to DirtyLocal
        // to simulate a post-sync local edit.
        task.mark_dirty_local().unwrap();
        task.set_body("new body".into());
        task_repo
            .save(&task, SnapshotSource::LocalEdit)
            .await
            .unwrap();

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

    // ---- Phase C: grace counter ------------------------------------------

    // Mutable probe local to these tests so we can flip path presence between
    // ticks for the "path returns" case. Kept here (rather than extended onto
    // `StubFilesystemProbe`) to honour the plan's "touches app-daemon only"
    // constraint.
    #[derive(Default)]
    struct MutableProbe {
        present: Mutex<HashSet<PathBuf>>,
    }

    impl MutableProbe {
        fn new() -> Self {
            Self::default()
        }
        fn add(&self, path: impl Into<PathBuf>) {
            self.present.lock().unwrap().insert(path.into());
        }
        fn remove(&self, path: impl AsRef<std::path::Path>) {
            self.present.lock().unwrap().remove(path.as_ref());
        }
    }

    #[async_trait]
    impl FilesystemProbe for MutableProbe {
        async fn path_exists(&self, path: &std::path::Path) -> ports::PortResult<bool> {
            Ok(self.present.lock().unwrap().contains(path))
        }
        async fn is_git_worktree(&self, _path: &std::path::Path) -> ports::PortResult<bool> {
            Ok(false)
        }
    }

    /// Build a daemon with one workspace + one binding linked to each path
    /// in `link_paths`. The probe parameter controls which paths report as
    /// present.
    ///
    /// IMPORTANT — the returned daemon has `prune` defaulted to `false` and
    /// `missing_grace_ticks = 3`. Tests that exercise the grace pass MUST
    /// chain `.with_prune(true)` (and usually `.with_missing_grace_ticks(N)`
    /// for the threshold under test). Forgetting `with_prune(true)` will
    /// silently produce `pruned = 0` and a counter map that stays empty,
    /// because `apply_grace_prune` is gated on `self.prune`.
    async fn seeded_grace_setup(
        probe: Arc<MutableProbe>,
        link_paths: &[&str],
    ) -> (Daemon, Arc<InMemoryRepoBindingRepository>, RepoId) {
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let bind_repo = Arc::new(InMemoryRepoBindingRepository::new());
        let task_repo = Arc::new(InMemoryTaskRepository::new());

        let ws = Workspace::new(WorkspaceName::new("scratch").unwrap(), None, true);
        ws_repo.save(&ws).await.unwrap();

        let mut binding = RepoBinding::new(
            ws.id,
            "git@github.com:o/r.git".into(),
            "github.com/o/r".into(),
        )
        .unwrap();
        for p in link_paths {
            binding.link_worktree(PathBuf::from(p), None);
        }
        let binding_id = binding.id;
        bind_repo.save(&binding).await.unwrap();

        let workspaces = WorkspaceService::new(ws_repo.clone());
        let bindings = RepoBindingService::new(ws_repo, bind_repo.clone());
        let probe_dyn: Arc<dyn FilesystemProbe> = probe;
        let daemon = Daemon::new(workspaces, bindings, task_repo, probe_dyn, None);
        (daemon, bind_repo, binding_id)
    }

    #[tokio::test]
    async fn grace_counter_defers_prune_until_threshold() {
        let probe = Arc::new(MutableProbe::new());
        // /tmp/gone is absent from the probe → marked missing on tick 1.
        let (daemon, bind_repo, bid) = seeded_grace_setup(probe.clone(), &["/tmp/gone"]).await;
        let daemon = daemon.with_prune(true).with_missing_grace_ticks(3);

        // Tick 1: marks missing + counter=1, no prune.
        let r1 = daemon.tick_once().await.unwrap();
        assert_eq!(r1.marked_missing, 1);
        assert_eq!(r1.pruned, 0);
        assert_eq!(
            *daemon.miss_counts.lock().unwrap().values().next().unwrap(),
            1
        );

        // Tick 2: counter=2, still defers.
        let r2 = daemon.tick_once().await.unwrap();
        assert_eq!(r2.marked_missing, 0);
        assert_eq!(r2.pruned, 0);
        assert_eq!(
            *daemon.miss_counts.lock().unwrap().values().next().unwrap(),
            2
        );

        // Tick 3: counter hits 3 → prune fires, entry GC'd.
        let r3 = daemon.tick_once().await.unwrap();
        assert_eq!(r3.pruned, 1);
        assert!(daemon.miss_counts.lock().unwrap().is_empty());

        // Verify the worktree is actually gone from the binding.
        let after = bind_repo.get(bid).await.unwrap();
        assert_eq!(after.worktrees.len(), 0);
    }

    #[tokio::test]
    async fn grace_counter_resets_when_path_returns() {
        let probe = Arc::new(MutableProbe::new());
        let (daemon, _bind_repo, _bid) = seeded_grace_setup(probe.clone(), &["/tmp/flicker"]).await;
        let daemon = daemon.with_prune(true).with_missing_grace_ticks(3);

        // Tick 1: missing → counter=1.
        let r1 = daemon.tick_once().await.unwrap();
        assert_eq!(r1.marked_missing, 1);
        assert_eq!(
            *daemon.miss_counts.lock().unwrap().values().next().unwrap(),
            1
        );

        // Path returns before tick 2. Counter resets.
        probe.add("/tmp/flicker");
        let r2 = daemon.tick_once().await.unwrap();
        assert_eq!(r2.pruned, 0);
        assert!(
            daemon.miss_counts.lock().unwrap().is_empty(),
            "counter must be empty after path returns"
        );

        // Path vanishes again — counter restarts at 1, not at 2.
        probe.remove("/tmp/flicker");
        let r3 = daemon.tick_once().await.unwrap();
        assert_eq!(r3.pruned, 0);
        assert_eq!(
            *daemon.miss_counts.lock().unwrap().values().next().unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn prune_disabled_skips_grace_pass() {
        let probe = Arc::new(MutableProbe::new());
        let (daemon, _bind_repo, _bid) =
            seeded_grace_setup(probe.clone(), &["/tmp/a", "/tmp/b", "/tmp/c"]).await;
        // prune=false (default); grace ticks irrelevant
        let daemon = daemon.with_missing_grace_ticks(2);

        for _ in 0..5 {
            let r = daemon.tick_once().await.unwrap();
            assert_eq!(r.pruned, 0);
        }
        // Counter is never touched when prune is off.
        assert!(daemon.miss_counts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn grace_ticks_one_is_legacy_behaviour() {
        let probe = Arc::new(MutableProbe::new());
        let (daemon, _bind_repo, _bid) = seeded_grace_setup(probe.clone(), &["/tmp/gone"]).await;
        let daemon = daemon.with_prune(true).with_missing_grace_ticks(1);

        // With threshold=1, first miss is enough to prune.
        let r = daemon.tick_once().await.unwrap();
        assert_eq!(r.marked_missing, 1);
        assert_eq!(r.pruned, 1);
        assert!(daemon.miss_counts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn grace_counter_survives_across_multiple_workspaces() {
        // Regression: each per-workspace `apply_grace_prune` must not GC
        // counters belonging to other workspaces. With the original bug,
        // workspace A's counter is dropped by workspace B's GC pass within
        // the same tick, so neither workspace ever reaches the threshold.
        let probe = Arc::new(MutableProbe::new());
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let bind_repo = Arc::new(InMemoryRepoBindingRepository::new());
        let task_repo = Arc::new(InMemoryTaskRepository::new());

        let ws_a = Workspace::new(WorkspaceName::new("alpha").unwrap(), None, true);
        let ws_b = Workspace::new(WorkspaceName::new("beta").unwrap(), None, true);
        ws_repo.save(&ws_a).await.unwrap();
        ws_repo.save(&ws_b).await.unwrap();

        let mut binding_a = RepoBinding::new(
            ws_a.id,
            "git@github.com:o/r-a.git".into(),
            "github.com/o/r-a".into(),
        )
        .unwrap();
        binding_a.link_worktree(PathBuf::from("/tmp/gone-a"), None);
        bind_repo.save(&binding_a).await.unwrap();

        let mut binding_b = RepoBinding::new(
            ws_b.id,
            "git@github.com:o/r-b.git".into(),
            "github.com/o/r-b".into(),
        )
        .unwrap();
        binding_b.link_worktree(PathBuf::from("/tmp/gone-b"), None);
        bind_repo.save(&binding_b).await.unwrap();

        let workspaces = WorkspaceService::new(ws_repo.clone());
        let bindings = RepoBindingService::new(ws_repo, bind_repo.clone());
        let probe_dyn: Arc<dyn FilesystemProbe> = probe;
        let daemon = Daemon::new(workspaces, bindings, task_repo, probe_dyn, None)
            .with_prune(true)
            .with_missing_grace_ticks(3);

        // Tick 1: marks both missing, both counters land at 1.
        daemon.tick_once().await.unwrap();
        {
            let counts = daemon.miss_counts.lock().unwrap();
            assert_eq!(counts.len(), 2, "both workspace counters must survive");
            assert!(
                counts.values().all(|&c| c == 1),
                "both counters should be at 1 after tick 1, got {:?}",
                counts.values().collect::<Vec<_>>()
            );
        }

        // Tick 2: both bump to 2.
        daemon.tick_once().await.unwrap();
        {
            let counts = daemon.miss_counts.lock().unwrap();
            assert_eq!(counts.len(), 2, "both workspace counters must persist");
            assert!(counts.values().all(|&c| c == 2));
        }

        // Tick 3: both hit threshold, both prune in the same tick.
        let r3 = daemon.tick_once().await.unwrap();
        assert_eq!(
            r3.pruned, 2,
            "both workspaces should prune at the same tick"
        );
        assert!(daemon.miss_counts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn grace_counters_advance_independently_across_workspaces() {
        // Late-added second workspace: counters for WS1 and WS2 advance on
        // their own schedules and prune at different ticks. Exercises the
        // tick-level GC merging `still_valid` across workspaces *and* the
        // independence of (RepoId, PathBuf) keys.
        let probe = Arc::new(MutableProbe::new());
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let bind_repo = Arc::new(InMemoryRepoBindingRepository::new());
        let task_repo = Arc::new(InMemoryTaskRepository::new());

        // T0: only WS1 exists, with a missing worktree.
        let ws1 = Workspace::new(WorkspaceName::new("alpha").unwrap(), None, true);
        ws_repo.save(&ws1).await.unwrap();
        let mut b1 = RepoBinding::new(
            ws1.id,
            "git@github.com:o/r1.git".into(),
            "github.com/o/r1".into(),
        )
        .unwrap();
        b1.link_worktree(PathBuf::from("/tmp/gone-1"), None);
        bind_repo.save(&b1).await.unwrap();

        let workspaces = WorkspaceService::new(ws_repo.clone());
        let bindings = RepoBindingService::new(ws_repo.clone(), bind_repo.clone());
        let probe_dyn: Arc<dyn FilesystemProbe> = probe;
        let daemon = Daemon::new(workspaces, bindings, task_repo, probe_dyn, None)
            .with_prune(true)
            .with_missing_grace_ticks(3);

        // Tick 1: WS1's counter lands at 1. WS2 doesn't exist yet.
        daemon.tick_once().await.unwrap();
        {
            let counts = daemon.miss_counts.lock().unwrap();
            assert_eq!(counts.len(), 1, "only WS1's key should be present");
            assert_eq!(*counts.values().next().unwrap(), 1);
        }

        // Between ticks: add WS2 with its own missing worktree.
        let ws2 = Workspace::new(WorkspaceName::new("beta").unwrap(), None, true);
        ws_repo.save(&ws2).await.unwrap();
        let mut b2 = RepoBinding::new(
            ws2.id,
            "git@github.com:o/r2.git".into(),
            "github.com/o/r2".into(),
        )
        .unwrap();
        b2.link_worktree(PathBuf::from("/tmp/gone-2"), None);
        bind_repo.save(&b2).await.unwrap();

        // Tick 2: WS1 → 2 (continues), WS2 → 1 (fresh start, doesn't inherit).
        daemon.tick_once().await.unwrap();
        {
            let counts = daemon.miss_counts.lock().unwrap();
            assert_eq!(counts.len(), 2);
            let mut vals: Vec<u32> = counts.values().copied().collect();
            vals.sort();
            assert_eq!(vals, vec![1, 2], "WS1=2 (continuing), WS2=1 (fresh)");
        }

        // Tick 3: WS1 hits threshold → prunes. WS2 → 2. Only one prune fires.
        let r3 = daemon.tick_once().await.unwrap();
        assert_eq!(
            r3.pruned, 1,
            "only WS1 should prune at tick 3; WS2 still at 2"
        );
        {
            let counts = daemon.miss_counts.lock().unwrap();
            assert_eq!(counts.len(), 1, "WS1's entry should be gone");
            assert_eq!(*counts.values().next().unwrap(), 2);
        }

        // Tick 4: WS2 hits threshold → prunes. Map empty.
        let r4 = daemon.tick_once().await.unwrap();
        assert_eq!(r4.pruned, 1, "WS2 should prune at tick 4");
        assert!(daemon.miss_counts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn tick_once_writes_last_tick_json() {
        // Heartbeat invariant: every tick_once leaves a parseable
        // last_tick.json in the configured state_dir, even with no
        // workspaces to do work over. `daemon status` reads this file to
        // decide "wedged or not?".
        let probe = Arc::new(StubFilesystemProbe::new());
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let bind_repo = Arc::new(InMemoryRepoBindingRepository::new());
        let task_repo = Arc::new(InMemoryTaskRepository::new());

        let ws = Workspace::new(WorkspaceName::new("hb").unwrap(), None, true);
        ws_repo.save(&ws).await.unwrap();

        let workspaces = WorkspaceService::new(ws_repo.clone());
        let bindings = RepoBindingService::new(ws_repo, bind_repo);

        let tmp = tempfile::TempDir::new().unwrap();
        let daemon = Daemon::new(workspaces, bindings, task_repo, probe, None)
            .with_state_dir(tmp.path().to_path_buf())
            .with_interval_secs(42);

        let before = chrono::Utc::now();
        let _ = daemon.tick_once().await.unwrap();
        let after = chrono::Utc::now();

        let path = tmp.path().join("last_tick.json");
        assert!(path.exists(), "last_tick.json was not written");
        let parsed: LastTick =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            parsed.tick_at >= before && parsed.tick_at <= after,
            "tick_at {} must fall within [{before}, {after}]",
            parsed.tick_at,
        );
        assert_eq!(parsed.interval_secs, 42);
        assert_eq!(parsed.report.workspaces, 1);
    }

    #[tokio::test]
    async fn grace_counter_garbage_collects_orphan_entries() {
        let probe = Arc::new(MutableProbe::new());
        // Healthy worktree: present in probe, will never get marked missing.
        probe.add("/tmp/healthy");
        let (daemon, _bind_repo, _bid) = seeded_grace_setup(probe.clone(), &["/tmp/healthy"]).await;
        let daemon = daemon.with_prune(true).with_missing_grace_ticks(3);

        // Pre-seed a ghost entry: a key that has no corresponding MissingPath
        // worktree (e.g. a binding that was deleted externally).
        let ghost_key = (RepoId::new(), PathBuf::from("/tmp/ghost"));
        daemon
            .miss_counts
            .lock()
            .unwrap()
            .insert(ghost_key.clone(), 42);
        assert_eq!(daemon.miss_counts.lock().unwrap().len(), 1);

        // One tick — grace pass runs (prune=true), GCs the orphan.
        let r = daemon.tick_once().await.unwrap();
        assert_eq!(r.pruned, 0);
        assert!(
            !daemon.miss_counts.lock().unwrap().contains_key(&ghost_key),
            "ghost entry should be GC'd"
        );
    }
}
