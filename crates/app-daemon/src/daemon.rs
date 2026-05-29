use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use application_sync::OutboxDrainer;
use application_workspace::{RepoBindingService, WorkspaceService};
use domain_core::RepoId;
use domain_sync::{OutboxEntry, OutboxMutation};
use domain_task::SyncState;
use dto_shared::UnlinkWorktreeCmd;
use ports::{FilesystemProbe, OutboxRepository, TaskFilter, TaskRepository};
use tokio::signal;
use tracing::{Instrument, error, info, info_span, warn};

use crate::error::DaemonError;
use crate::report::{TickReport, write_last_tick_atomic};

/// Serde-serialised form of `domain_repo::LinkStatus::MissingPath` —
/// the enum carries `#[serde(rename_all = "snake_case")]`, so the DTO
/// surfaces it as this literal. Defined once here so a future rename of
/// the enum (or a casing change on the serde rename) doesn't silently
/// disable the grace-prune match. The daemon's unit tests round-trip a
/// real binding through the DTO mapping, so drift between this constant
/// and the actual serialised form fails the test suite on the next run.
const STATUS_MISSING_PATH: &str = "missing_path";

pub struct Daemon {
    workspaces: WorkspaceService,
    bindings: RepoBindingService,
    tasks: Arc<dyn TaskRepository>,
    probe: Arc<dyn FilesystemProbe>,
    /// The sole outbound path (RFC 0001 Stage 6 cutover, #54). `None` when no
    /// GitHub token is configured — the daemon then only reconciles worktrees.
    /// Replaces the former direct `SyncService::push` loop: lifecycle / edit
    /// verbs enqueue outbox entries (in `application-task` /
    /// `application-workspace`); the daemon drains them here.
    drainer: Option<Arc<OutboxDrainer>>,
    /// Outbox handle for the one-time startup reconcile (see
    /// [`Self::reconcile_dirty_into_outbox`]). Shares the same repo the
    /// drainer reads.
    outbox: Option<Arc<dyn OutboxRepository>>,
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
        drainer: Option<Arc<OutboxDrainer>>,
        outbox: Option<Arc<dyn OutboxRepository>>,
    ) -> Self {
        Self {
            workspaces,
            bindings,
            tasks,
            probe,
            drainer,
            outbox,
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

                Ok::<(), DaemonError>(())
            }
            .instrument(info_span!("workspace_tick", workspace_id = %ws.id, name = %ws.name))
            .await?;
        }

        // Outbound work is the drainer's job (Stage 6 cutover, #54). It runs
        // once per tick across ALL tasks — outbox entries are enqueued by the
        // lifecycle / edit verbs, not scanned for here. This replaces the
        // former per-workspace `DirtyLocal → SyncService::push` loop; there is
        // no double-write because the drainer is now the only outbound path the
        // daemon drives (`rl task claim` keeps its own inline push for
        // interactive feedback — a deliberate exception).
        if let Some(drainer) = &self.drainer {
            match drainer.drain_once().await {
                Ok(n) => {
                    report.pushed += n;
                    info!(drained = n, "outbox drained");
                }
                Err(e) => {
                    warn!(error = %e, "outbox drain failed");
                    report.push_failures.push(format!("drain: {e}"));
                }
            }
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

    /// Recover entries orphaned `inflight` by a previous run (#54). The claim
    /// flips an entry to `inflight` in a committed transaction *before* the
    /// drainer applies it and resolves it to succeeded / pending / failed. If
    /// the daemon crashed / was killed / OOM'd in that window, the entry is
    /// stranded `inflight`: no reaper ever resolves it, and because the
    /// per-task-FIFO guard keys on inflight rows, the task's whole pending tail
    /// is blocked forever. Reset every such row back to `pending` (eligible
    /// immediately) on startup. Safe because the daemon is single-instance — at
    /// startup nothing is legitimately inflight. Runs once before the run-loop
    /// AND before [`Self::reconcile_dirty_into_outbox`], so the reconcile's
    /// pending-guard sees the recovered rows and doesn't double-enqueue.
    pub async fn requeue_orphaned_inflight(&self) -> Result<usize, DaemonError> {
        let Some(outbox) = &self.outbox else {
            return Ok(0);
        };
        let reset = outbox
            .requeue_orphaned_inflight()
            .await
            .map_err(|e| DaemonError::Sync(e.to_string()))?;
        if reset > 0 {
            info!(reset, "startup: requeued orphaned inflight outbox entries");
        }
        Ok(reset)
    }

    /// One-time startup reconcile (#54). Tasks that were already `DirtyLocal`
    /// at the moment the daemon upgraded to the outbox path never had an entry
    /// enqueued (the lifecycle verbs only enqueue going forward). Without this,
    /// they'd never drain. So on startup, for every `DirtyLocal` mirror task
    /// that has **no** unresolved (pending *or* inflight) outbox entry **and no
    /// dead-lettered one**, enqueue an `UpdateRemote`.
    ///
    /// Skips tasks that already have an unresolved entry (the common forward
    /// path) so it never double-enqueues; a task with no remote / repo can't
    /// form an `UpdateRemote` so it's skipped too. A dead-lettered (`failed`)
    /// entry is *also* a blocker: such a task stays `DirtyLocal`, so without
    /// this guard the next restart would enqueue a brand-new `UpdateRemote`,
    /// silently bypassing the attempt cap and retrying forever across restarts.
    /// Runs once before the run-loop, after [`Self::requeue_orphaned_inflight`]
    /// has reset any stranded inflight rows back to pending — so by the time
    /// this runs there are no inflight rows, and the dedupe guard below is
    /// exhaustive. Not per tick.
    ///
    /// The dedupe check + enqueue is atomic: it goes through
    /// [`OutboxRepository::enqueue_if_absent`], which evaluates the
    /// `pending`/`inflight`/`failed` guard and inserts under one transaction,
    /// so a concurrent CLI edit can't enqueue a `pending` row in the window
    /// between a separate check and a separate insert (which would produce a
    /// duplicate `UpdateRemote` for the task) (#54).
    pub async fn reconcile_dirty_into_outbox(&self) -> Result<usize, DaemonError> {
        let Some(outbox) = &self.outbox else {
            return Ok(0);
        };
        let dirty = self
            .tasks
            .list(TaskFilter {
                sync_state: Some(SyncState::DirtyLocal),
                include_archived: true,
                ..TaskFilter::default()
            })
            .await?;
        let mut enqueued = 0usize;
        for t in dirty {
            // A task with no remote / repo can't form an `UpdateRemote`, so
            // skip it before touching the outbox.
            let Some(remote) = t.remote.as_ref() else {
                continue;
            };
            let Some(repo_id) = t.repo_id else {
                continue;
            };
            let canonical = match self.bindings.show(&repo_id.to_string()).await {
                Ok(b) => b.canonical_url,
                Err(e) => {
                    warn!(task_id = %t.id, error = %e, "startup reconcile: binding lookup failed");
                    continue;
                }
            };
            let entry = OutboxEntry::new(
                t.id,
                OutboxMutation::UpdateRemote {
                    canonical_repo: canonical,
                    remote_id: remote.remote_id.clone(),
                    title: Some(t.title.clone()),
                    body: Some(t.body.clone()),
                    closed: None,
                },
            );
            // Atomic dedupe + enqueue (#54): the insert lands only if the task
            // has no non-terminal (`pending` / `inflight`) and no dead-lettered
            // (`failed`) sibling — collapsing the former `list_pending` +
            // `list_failed` + `enqueue` round-trips into ONE transaction so a
            // concurrent CLI edit can't slip a `pending` row in between the
            // checks and the insert. The pending/inflight guard is the common
            // forward-path skip; the failed guard keeps a dead-letter terminal
            // (re-enqueuing would silently bypass the attempt cap and retry
            // forever across restarts). Inflight rows were reset to pending by
            // the requeue pass that runs first, so the guard is exhaustive at
            // startup.
            let inserted = outbox
                .enqueue_if_absent(&entry)
                .await
                .map_err(|e| DaemonError::Sync(e.to_string()))?;
            if inserted {
                enqueued += 1;
            }
        }
        if enqueued > 0 {
            info!(
                enqueued,
                "startup reconcile enqueued dirty tasks into outbox"
            );
        }
        Ok(enqueued)
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
        // Crash recovery first: reset any entries stranded `inflight` by a
        // previous run back to `pending` so they (and their blocked tail)
        // drain again (#54). Must precede the reconcile so its pending-guard
        // sees the recovered rows. A failure here is logged, not fatal.
        if let Err(e) = self.requeue_orphaned_inflight().await {
            warn!(error = %e, "startup inflight requeue failed");
        }

        // One-time upgrade reconcile: drain any tasks that were already
        // DirtyLocal before the outbox path existed (#54). A failure here is
        // logged but doesn't abort startup — the next forward edit re-enqueues.
        if let Err(e) = self.reconcile_dirty_into_outbox().await {
            warn!(error = %e, "startup outbox reconcile failed");
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::LastTick;
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
        InMemoryOutboxRepository, InMemoryProjectRepository, InMemoryRemoteProjectProvider,
        InMemoryRepoBindingRepository, InMemoryTaskRepository, InMemoryWorkspaceRepository,
        StubFilesystemProbe,
    };

    #[derive(Default)]
    struct CountingProvider {
        updates: AtomicUsize,
    }

    #[async_trait]
    impl RemoteTaskProvider for CountingProvider {
        async fn create_remote(&self, cmd: RemoteTaskCreate<'_>) -> PortResult<RemoteTaskSnapshot> {
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
    async fn tick_reconciles_and_drains_outbox_without_double_write() {
        // Stage-6 cutover (#54): the daemon's outbound path is the drainer.
        // A single enqueued UpdateRemote drains exactly once; a second tick
        // does NOT re-fire it (the entry is now `succeeded`, not pending) —
        // so there's no duplicate DirtyLocal push.
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let bind_repo = Arc::new(InMemoryRepoBindingRepository::new());
        let task_repo = Arc::new(InMemoryTaskRepository::new());
        let proj_repo = Arc::new(InMemoryProjectRepository::new());
        let outbox = Arc::new(InMemoryOutboxRepository::new());
        let provider = Arc::new(CountingProvider::default());
        let projects_provider = Arc::new(InMemoryRemoteProjectProvider::new());

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

        // A task that was synced + then locally edited (DirtyLocal mirror).
        let mut task = Task::new_draft(ws.id, Some(binding.id), "edit me".into()).unwrap();
        task.stage_for_sync().unwrap();
        task.promote_to_remote(domain_task::RemoteRef::new("github", "777"))
            .unwrap();
        task.mark_dirty_local().unwrap();
        task.set_body("new body".into());
        task_repo
            .save(&task, SnapshotSource::LocalEdit)
            .await
            .unwrap();

        // The forward path would have enqueued this on the local edit; seed it
        // directly here since the test mutates the task aggregate by hand.
        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "777".into(),
                title: Some(task.title.clone()),
                body: Some(task.body.clone()),
                closed: None,
            },
        );
        outbox.enqueue(&entry).await.unwrap();

        let workspaces = WorkspaceService::new(ws_repo.clone());
        let bindings = RepoBindingService::new(ws_repo.clone(), bind_repo.clone());
        let remote_tasks: Arc<dyn RemoteTaskProvider> = provider.clone();
        let remote_projects: Arc<dyn ports::RemoteProjectProvider> = projects_provider.clone();
        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let drainer = Arc::new(OutboxDrainer::new(
            outbox_dyn.clone(),
            task_repo.clone(),
            ws_repo.clone(),
            proj_repo.clone(),
            remote_tasks,
            remote_projects,
        ));
        let daemon = Daemon::new(
            workspaces,
            bindings,
            task_repo.clone(),
            probe.clone(),
            Some(drainer),
            Some(outbox_dyn),
        );

        let report = daemon.tick_once().await.unwrap();
        assert_eq!(report.workspaces, 1);
        assert_eq!(report.repos_checked, 1);
        assert_eq!(report.worktrees_checked, 2);
        assert_eq!(report.marked_missing, 1);
        assert_eq!(report.pruned, 0);
        assert_eq!(report.pushed, 1, "the one outbox entry drained");
        assert!(report.push_failures.is_empty());
        assert_eq!(provider.updates.load(Ordering::SeqCst), 1);

        // Second tick: the entry is succeeded, nothing newly missing — no
        // duplicate remote write.
        let report = daemon.tick_once().await.unwrap();
        assert_eq!(report.marked_missing, 0);
        assert_eq!(report.pushed, 0);
        assert_eq!(
            provider.updates.load(Ordering::SeqCst),
            1,
            "no duplicate remote write on the second tick"
        );
    }

    #[tokio::test]
    async fn startup_requeues_orphaned_inflight_then_reconcile_does_not_double_enqueue() {
        // A previous run crashed with an entry stranded `inflight`. On startup
        // `requeue_orphaned_inflight` resets it to pending; the subsequent
        // dirty reconcile then sees a pending entry for that task and does NOT
        // enqueue a duplicate. Net: exactly one outbox entry survives.
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let bind_repo = Arc::new(InMemoryRepoBindingRepository::new());
        let task_repo = Arc::new(InMemoryTaskRepository::new());
        let outbox = Arc::new(InMemoryOutboxRepository::new());

        let ws = Workspace::new(WorkspaceName::new("scratch").unwrap(), None, true);
        ws_repo.save(&ws).await.unwrap();
        let mut binding = RepoBinding::new(
            ws.id,
            "git@github.com:o/r.git".into(),
            "github.com/o/r".into(),
        )
        .unwrap();
        binding.link_worktree(std::path::PathBuf::from("/tmp/exists"), None);
        bind_repo.save(&binding).await.unwrap();

        // A DirtyLocal issue-backed mirror.
        let mut task = Task::new_draft(ws.id, Some(binding.id), "edit me".into()).unwrap();
        task.stage_for_sync().unwrap();
        task.promote_to_remote(domain_task::RemoteRef::new("github", "777"))
            .unwrap();
        task.mark_dirty_local().unwrap();
        task.set_body("new body".into());
        task_repo
            .save(&task, SnapshotSource::LocalEdit)
            .await
            .unwrap();

        // Its outbox entry is stranded `inflight` (claimed but never resolved
        // before the crash).
        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "777".into(),
                title: Some(task.title.clone()),
                body: Some(task.body.clone()),
                closed: None,
            },
        );
        outbox.enqueue(&entry).await.unwrap();
        let _ = outbox
            .claim_next_eligible(domain_core::Timestamp::now())
            .await
            .unwrap()
            .expect("claimed → now inflight");
        assert_eq!(
            outbox.all()[0].status,
            domain_sync::OutboxStatus::Inflight,
            "precondition: entry is inflight"
        );

        let probe = Arc::new(StubFilesystemProbe::new().with_path("/tmp/exists"));
        let workspaces = WorkspaceService::new(ws_repo.clone());
        let bindings = RepoBindingService::new(ws_repo.clone(), bind_repo.clone());
        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let daemon = Daemon::new(
            workspaces,
            bindings,
            task_repo.clone(),
            probe,
            None,
            Some(outbox_dyn),
        );

        // Startup recovery resets the inflight row to pending.
        let reset = daemon.requeue_orphaned_inflight().await.unwrap();
        assert_eq!(reset, 1, "the orphaned inflight entry was requeued");
        assert_eq!(
            outbox.all()[0].status,
            domain_sync::OutboxStatus::Pending,
            "entry is pending again, eligible immediately"
        );

        // The dirty reconcile now finds a pending entry and skips → no dup.
        let enqueued = daemon.reconcile_dirty_into_outbox().await.unwrap();
        assert_eq!(enqueued, 0, "reconcile must not double-enqueue");
        assert_eq!(outbox.all().len(), 1, "exactly one outbox entry survives");
    }

    #[tokio::test]
    async fn startup_reconcile_skips_a_dead_lettered_task() {
        // Regression (#54): a task whose outbox entry already dead-lettered
        // (`failed`) is still DirtyLocal. The startup reconcile must NOT
        // enqueue a brand-new UpdateRemote for it — doing so would silently
        // bypass the attempt cap and retry forever across restarts.
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let bind_repo = Arc::new(InMemoryRepoBindingRepository::new());
        let task_repo = Arc::new(InMemoryTaskRepository::new());
        let outbox = Arc::new(InMemoryOutboxRepository::new());

        let ws = Workspace::new(WorkspaceName::new("scratch").unwrap(), None, true);
        ws_repo.save(&ws).await.unwrap();
        let mut binding = RepoBinding::new(
            ws.id,
            "git@github.com:o/r.git".into(),
            "github.com/o/r".into(),
        )
        .unwrap();
        binding.link_worktree(std::path::PathBuf::from("/tmp/exists"), None);
        bind_repo.save(&binding).await.unwrap();

        // A DirtyLocal issue-backed mirror.
        let mut task = Task::new_draft(ws.id, Some(binding.id), "edit me".into()).unwrap();
        task.stage_for_sync().unwrap();
        task.promote_to_remote(domain_task::RemoteRef::new("github", "777"))
            .unwrap();
        task.mark_dirty_local().unwrap();
        task.set_body("new body".into());
        task_repo
            .save(&task, SnapshotSource::LocalEdit)
            .await
            .unwrap();

        // Its outbox entry dead-letters: enqueue, claim (→ inflight), then
        // mark_failed (the drainer's terminal dead-letter at the attempt cap).
        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "777".into(),
                title: Some(task.title.clone()),
                body: Some(task.body.clone()),
                closed: None,
            },
        );
        outbox.enqueue(&entry).await.unwrap();
        let claimed = outbox
            .claim_next_eligible(domain_core::Timestamp::now())
            .await
            .unwrap()
            .expect("claimed → inflight");
        outbox.mark_failed(claimed.id, "boom").await.unwrap();
        assert_eq!(
            outbox.all()[0].status,
            domain_sync::OutboxStatus::Failed,
            "precondition: entry is dead-lettered"
        );

        let probe = Arc::new(StubFilesystemProbe::new().with_path("/tmp/exists"));
        let workspaces = WorkspaceService::new(ws_repo.clone());
        let bindings = RepoBindingService::new(ws_repo.clone(), bind_repo.clone());
        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let daemon = Daemon::new(
            workspaces,
            bindings,
            task_repo.clone(),
            probe,
            None,
            Some(outbox_dyn),
        );

        // The reconcile must skip the dead-lettered task: no new entry.
        let enqueued = daemon.reconcile_dirty_into_outbox().await.unwrap();
        assert_eq!(enqueued, 0, "a dead-lettered task is not re-enqueued");
        assert_eq!(
            outbox.all().len(),
            1,
            "only the dead-lettered entry exists; no new UpdateRemote"
        );
    }

    #[tokio::test]
    async fn startup_reconcile_does_not_duplicate_when_pending_entry_exists() {
        // Atomic dedupe (#54): a DirtyLocal task that already carries a pending
        // outbox entry must NOT get a second one from the startup reconcile.
        // The reconcile now routes through `enqueue_if_absent`, which checks the
        // pending/inflight/failed guard and inserts in one transaction — so even
        // the forward-path skip can't be raced into a duplicate. Assert exactly
        // one entry survives.
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let bind_repo = Arc::new(InMemoryRepoBindingRepository::new());
        let task_repo = Arc::new(InMemoryTaskRepository::new());
        let outbox = Arc::new(InMemoryOutboxRepository::new());

        let ws = Workspace::new(WorkspaceName::new("scratch").unwrap(), None, true);
        ws_repo.save(&ws).await.unwrap();
        let mut binding = RepoBinding::new(
            ws.id,
            "git@github.com:o/r.git".into(),
            "github.com/o/r".into(),
        )
        .unwrap();
        binding.link_worktree(std::path::PathBuf::from("/tmp/exists"), None);
        bind_repo.save(&binding).await.unwrap();

        // A DirtyLocal issue-backed mirror with an already-pending outbox entry
        // (the forward path enqueued it on the local edit).
        let mut task = Task::new_draft(ws.id, Some(binding.id), "edit me".into()).unwrap();
        task.stage_for_sync().unwrap();
        task.promote_to_remote(domain_task::RemoteRef::new("github", "777"))
            .unwrap();
        task.mark_dirty_local().unwrap();
        task.set_body("new body".into());
        task_repo
            .save(&task, SnapshotSource::LocalEdit)
            .await
            .unwrap();

        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "777".into(),
                title: Some(task.title.clone()),
                body: Some(task.body.clone()),
                closed: None,
            },
        );
        outbox.enqueue(&entry).await.unwrap();

        let probe = Arc::new(StubFilesystemProbe::new().with_path("/tmp/exists"));
        let workspaces = WorkspaceService::new(ws_repo.clone());
        let bindings = RepoBindingService::new(ws_repo.clone(), bind_repo.clone());
        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let daemon = Daemon::new(
            workspaces,
            bindings,
            task_repo.clone(),
            probe,
            None,
            Some(outbox_dyn),
        );

        let enqueued = daemon.reconcile_dirty_into_outbox().await.unwrap();
        assert_eq!(
            enqueued, 0,
            "an existing pending entry must not be duplicated"
        );
        assert_eq!(outbox.all().len(), 1, "exactly one outbox entry survives");
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
        let daemon = Daemon::new(workspaces, bindings, task_repo, probe, None, None);

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
        let daemon = Daemon::new(workspaces, bindings, task_repo, probe_dyn, None, None);
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
        let daemon = Daemon::new(workspaces, bindings, task_repo, probe_dyn, None, None)
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
        let daemon = Daemon::new(workspaces, bindings, task_repo, probe_dyn, None, None)
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
        let daemon = Daemon::new(workspaces, bindings, task_repo, probe, None, None)
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
