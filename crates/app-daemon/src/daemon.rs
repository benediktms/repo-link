use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use application_sync::{OutboxDrainer, ProjectPoller};
use application_workspace::{RepoBindingService, WorkspaceService};
use domain_core::RepoId;
use domain_sync::{OutboxEntry, OutboxMutation};
use domain_task::SyncState;
use dto_shared::UnlinkWorktreeCmd;
use ports::{FilesystemProbe, OutboxRepository, TaskFilter, TaskRepository};
use tokio::signal;
use tokio::sync::{Notify, watch};
use tracing::{Instrument, error, info, info_span, warn};

use crate::error::DaemonError;
use crate::report::{TickReport, write_last_tick_atomic};

/// Cadence of the poller task: project poll, worktree reconcile, grace-prune,
/// and heartbeat all share this tick (RFC 0001 Stage 7 §7c, §D4 30–60s band).
/// This task writes the single combined `last_tick.json`, so it stays the
/// primary cadence `rl daemon status` measures "wedged" against. Used as the
/// default for `--interval-secs` (which still overrides it); `run` is handed
/// the resolved cadence so tests can shorten it.
// TODO(config): expose via infra-config once a user actually asks.
pub const PROJECT_POLLER_INTERVAL: Duration = Duration::from_secs(45);

/// Periodic safety-net sweep for the drainer task (RFC 0001 Stage 7 §7c). The
/// drainer's primary trigger is just-in-time via a `tokio::sync::Notify`, but
/// that `Notify` only fires for *in-process* (daemon-originated) enqueues. A
/// `rl task start/edit/complete` runs in a SEPARATE process and enqueues
/// straight into the shared SQLite outbox — the daemon never sees that signal.
/// This 5s sweep is what catches those cross-process enqueues.
// TODO(config): expose via infra-config once a user actually asks.
const OUTBOX_DRAINER_PERIODIC_SWEEP: Duration = Duration::from_secs(5);

/// Upper bound on how long shutdown waits for a task to observe cancellation
/// and unwind after the cancel signal is sent (Stage 7, #55). The task bodies
/// return promptly via their `cancel` arm, but a `tick_once` / `drain_tick`
/// stalled in network I/O (including the panic-triggered shutdown path, where
/// the surviving task may be mid-`.await`) would otherwise make the post-cancel
/// `JoinHandle.await` hang forever. After this grace elapses the outstanding
/// handle is `.abort()`ed so shutdown is guaranteed to complete.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

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
    /// Inbound counterpart to the drainer (Stage 7, #55): polls each known
    /// project and correlates items with local tasks. `None` when no GitHub
    /// token is configured (same gate as the drainer) — the daemon then only
    /// reconciles worktrees.
    poller: Option<Arc<ProjectPoller>>,
    /// Just-in-time wake for the drainer task. Daemon-originated enqueues
    /// (currently the startup reconcile) call `notify_one`; CLI-originated
    /// enqueues cross the process boundary and are caught by the periodic
    /// sweep instead. Always present so callers needn't branch on the token.
    drainer_notify: Arc<Notify>,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspaces: WorkspaceService,
        bindings: RepoBindingService,
        tasks: Arc<dyn TaskRepository>,
        probe: Arc<dyn FilesystemProbe>,
        drainer: Option<Arc<OutboxDrainer>>,
        poller: Option<Arc<ProjectPoller>>,
        outbox: Option<Arc<dyn OutboxRepository>>,
    ) -> Self {
        Self {
            workspaces,
            bindings,
            tasks,
            probe,
            drainer,
            poller,
            drainer_notify: Arc::new(Notify::new()),
            outbox,
            prune: false,
            missing_grace_ticks: 3,
            miss_counts: Mutex::new(HashMap::new()),
            state_dir: None,
            interval_secs: 60,
        }
    }

    /// Handle to the drainer's just-in-time wake. The CLI process can't reach
    /// this (it enqueues across the process boundary), but in-process callers
    /// — and tests — use it to nudge the drainer awake without waiting for the
    /// periodic sweep.
    pub fn drainer_notify(&self) -> Arc<Notify> {
        self.drainer_notify.clone()
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

    /// One full reconcile pass: worktree reconcile + grace-prune (when
    /// enabled) + project poll + heartbeat. This is the **poller task's** unit
    /// of work in the two-task split (Stage 7, #55) — outbound draining is the
    /// separate drainer task's job (see [`Self::drain_tick`]), so `tick_once`
    /// no longer drains. It still writes the single combined `last_tick.json`
    /// so `rl daemon status` keeps one primary cadence to measure against.
    /// Returns counts so callers can log progress and tests can assert.
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

        // Inbound work: poll each known project and correlate items with local
        // tasks (Stage 7, #55). Folded into this task so the poll, reconcile,
        // and heartbeat share one cadence. A poll failure is non-fatal — it's
        // logged and the next cycle retries; correctness lives in the poller's
        // own per-project skip-and-continue. (Outbound draining is the separate
        // drainer task — see `Self::drain_tick` — so it is intentionally NOT
        // called here; doing both here would double-drive the outbox.)
        if let Some(poller) = &self.poller {
            match poller.poll_once().await {
                Ok(p) => info!(
                    projects = p.projects_polled,
                    items = p.items_seen,
                    matched = p.items_matched,
                    "project poll complete"
                ),
                Err(e) => warn!(error = %e, "project poll failed"),
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

    /// The **drainer task's** unit of work: one `OutboxDrainer::drain_once`
    /// pass over ALL tasks (Stage 6 cutover, #54). Outbox entries are enqueued
    /// by the lifecycle / edit verbs (in `application-task` /
    /// `application-workspace`) — not scanned for here. Returns the count
    /// drained so the loop can log; a drain error is non-fatal (logged by the
    /// caller, then the next sweep / notify retries). No-ops when no GitHub
    /// token is configured (`self.drainer` is `None`).
    ///
    /// `rl task claim` keeps its own inline synchronous push for interactive
    /// feedback — a deliberate, documented exception to "the drainer is the
    /// only outbound path the daemon drives."
    pub async fn drain_tick(&self) -> Result<usize, DaemonError> {
        let Some(drainer) = &self.drainer else {
            return Ok(0);
        };
        let n = drainer
            .drain_once()
            .await
            .map_err(|e| DaemonError::Sync(e.to_string()))?;
        if n > 0 {
            info!(drained = n, "outbox drained");
        }
        Ok(n)
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

    /// Drive the daemon until a shutdown signal (SIGINT / SIGTERM) or a panic
    /// in either background task.
    ///
    /// Stage 7 (#55) restructures this from a single ticker into **two
    /// concurrent `tokio::spawn`'d tasks** coordinated by a shared
    /// `tokio::sync::watch<bool>` cancellation (no `tokio-util`):
    ///
    /// - **Poller task** (`PROJECT_POLLER_INTERVAL`): runs [`Self::tick_once`]
    ///   — project poll + worktree reconcile + grace-prune + the single
    ///   combined heartbeat. Its cadence is the one `rl daemon status` measures
    ///   "wedged" against.
    /// - **Drainer task**: a `select!` over (a) the just-in-time
    ///   `drainer_notify` and (b) a `OUTBOX_DRAINER_PERIODIC_SWEEP` ticker;
    ///   either wake runs [`Self::drain_tick`]. The notify only fires for
    ///   in-process enqueues; the sweep is what catches CLI-originated
    ///   (cross-process) enqueues.
    ///
    /// Each task does its FIRST unit of work IMMEDIATELY on startup, then
    /// settles into its cadence — no warm-up `tick()` eats the first run (#88).
    ///
    /// Shutdown / join: a `tokio::select!` waits on the two `JoinHandle`s plus
    /// the shutdown signal. A handle resolving to `Err` (a `JoinError`, i.e. a
    /// panic) trips the shared cancellation so the *other* task also stops; a
    /// clean return is not a panic and is allowed to complete. Per-task tick
    /// errors stay non-fatal (logged, loop continues) — only a panic trips
    /// global shutdown. `interval` overrides the poller cadence (used by tests
    /// and honoured for `--interval-secs`); the drainer sweep is fixed.
    pub async fn run(self: Arc<Self>, interval: Duration) -> Result<(), DaemonError> {
        self.run_until(interval, shutdown_signal()).await
    }

    /// [`Self::run`] with an injectable shutdown future. Production passes
    /// [`shutdown_signal`] (SIGINT / SIGTERM); tests pass their own future
    /// (e.g. a `Notify::notified()`) to drive a clean shutdown deterministically
    /// without raising real process signals. All the spawn / join / panic
    /// semantics live here so both paths share them.
    pub async fn run_until(
        self: Arc<Self>,
        interval: Duration,
        shutdown: impl std::future::Future<Output = ()> + Send,
    ) -> Result<(), DaemonError> {
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
        // The startup reconcile is a daemon-originated enqueue, so wake the
        // drainer immediately rather than waiting for its first sweep.
        self.drainer_notify.notify_one();

        // Shared cancellation. `true` = shut down. Both tasks watch it; the
        // join loop flips it on a panic so the surviving task stops too.
        let (cancel_tx, cancel_rx) = watch::channel(false);

        let mut poller_task = {
            let daemon = self.clone();
            let mut cancel = cancel_rx.clone();
            tokio::spawn(async move { daemon.run_poller_task(interval, &mut cancel).await })
        };

        let mut drainer_task = {
            let daemon = self.clone();
            let mut cancel = cancel_rx.clone();
            tokio::spawn(async move { daemon.run_drainer_task(&mut cancel).await })
        };

        // Join + shutdown. Any of: the shutdown future resolves; OR a task ends
        // (its `JoinHandle` resolves). The join loop treats ANY task return as
        // "trip global shutdown", which is only correct because each task body
        // is an infinite loop that returns ONLY via its cancel arm (see the
        // invariant comments on `run_poller_task` / `run_drainer_task`). So a
        // handle resolving while `*cancel_rx.borrow()` is still `false` means
        // the task exited on its own, unexpectedly — either a panic
        // (`Err(JoinError)`) or a clean-but-spurious `return`; both are loud
        // `error!`s here, distinguished from the normal cancel-driven exit
        // (cancel already `true`). The `&mut handle` arms borrow the handles so
        // the one that *didn't* fire is still ownable below — a completed
        // `JoinHandle` must never be awaited twice.
        let mut poller_done = false;
        let mut drainer_done = false;
        tokio::pin!(shutdown);
        tokio::select! {
            _ = &mut shutdown => {
                info!("shutdown requested; stopping daemon tasks");
            }
            res = &mut poller_task => {
                poller_done = true;
                report_unexpected_task_exit("poller", res, &cancel_rx);
            }
            res = &mut drainer_task => {
                drainer_done = true;
                report_unexpected_task_exit("drainer", res, &cancel_rx);
            }
        }
        // Whatever woke us, signal both tasks to stop and await only the
        // handles that haven't already resolved — but bound the wait. A task
        // stalled in I/O (e.g. a `tick_once` / `drain_tick` mid network call,
        // including the panic-triggered shutdown path where the surviving task
        // is mid-`.await`) would otherwise hang this join forever. If the grace
        // elapses, `.abort()` the outstanding handle(s) so shutdown always
        // completes. A clean cancel-driven exit resolves well within the grace,
        // so the abort path is a safety net, not the normal route.
        let _ = cancel_tx.send(true);
        join_with_grace(poller_done, poller_task, drainer_done, drainer_task).await;
        Ok(())
    }

    /// Poller task body: tick once immediately (#88), then on every
    /// `interval` until cancelled. A tick error is logged, not fatal.
    ///
    /// INVARIANT: this MUST NOT return except via the `cancel` arm. The
    /// `run_until` join loop treats ANY return from this task as a signal to
    /// trip global shutdown, so a stray `return` / `break` would silently take
    /// the whole daemon down. Tick errors are therefore logged and the loop
    /// continues — never propagated out.
    async fn run_poller_task(&self, interval: Duration, cancel: &mut watch::Receiver<bool>) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first `tick()` on a fresh interval returns immediately, so this
        // performs the first unit of work right away (no warm-up tick eating
        // it — #88).
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
                            "poller tick complete"
                        ),
                        Err(e) => error!(error = %e, "poller tick failed"),
                    }
                }
                _ = cancel.changed() => {
                    if *cancel.borrow() {
                        info!("poller task stopping");
                        return;
                    }
                }
            }
        }
    }

    /// Drainer task body: drain once immediately (#88), then on either the
    /// just-in-time `drainer_notify` or the periodic sweep until cancelled. A
    /// drain error is logged, not fatal.
    ///
    /// INVARIANT: this MUST NOT return except via the `cancel` arm. The
    /// `run_until` join loop treats ANY return from this task as a signal to
    /// trip global shutdown, so a stray `return` / `break` would silently take
    /// the whole daemon down. Drain errors are therefore logged and the loop
    /// continues — never propagated out.
    ///
    /// Drop-mid-drain: a cancellation that lands while a `drain_tick` /
    /// `drain_once` future is suspended mid-`apply` simply drops that future
    /// (the `cancel` arm wins the `select!`). The drainer claims one entry at a
    /// time, so at most one entry is left stranded `inflight` by such a drop.
    /// That is recovered on the next startup by
    /// [`Self::requeue_orphaned_inflight`], which resets stranded `inflight`
    /// rows back to `pending`. In other words graceful shutdown deliberately
    /// shares the crash-recovery path rather than draining-to-quiescence here.
    async fn run_drainer_task(&self, cancel: &mut watch::Receiver<bool>) {
        let mut sweep = tokio::time::interval(OUTBOX_DRAINER_PERIODIC_SWEEP);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First sweep `tick()` returns immediately → drain right away (#88).
        loop {
            tokio::select! {
                _ = sweep.tick() => {
                    if let Err(e) = self.drain_tick().await {
                        error!(error = %e, "drainer sweep failed");
                    }
                }
                _ = self.drainer_notify.notified() => {
                    if let Err(e) = self.drain_tick().await {
                        error!(error = %e, "drainer notify-driven drain failed");
                    }
                }
                _ = cancel.changed() => {
                    if *cancel.borrow() {
                        info!("drainer task stopping");
                        return;
                    }
                }
            }
        }
    }
}

/// Await the still-outstanding task handles after cancellation, bounded by
/// [`SHUTDOWN_GRACE`]. Each unfinished handle gets the remaining grace to
/// unwind on its own; whatever has not resolved when the grace elapses is
/// `.abort()`ed so `run_until` can never hang on a task stalled in I/O. Handles
/// already resolved (their `*_done` flag set) are skipped — a completed
/// `JoinHandle` must never be awaited twice.
async fn join_with_grace(
    poller_done: bool,
    poller_task: tokio::task::JoinHandle<()>,
    drainer_done: bool,
    drainer_task: tokio::task::JoinHandle<()>,
) {
    // One shared deadline across both joins: the grace is a total budget for
    // shutdown, not per-task, so a slow first task can't double the wait.
    let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
    join_one_with_deadline("poller", poller_done, poller_task, deadline).await;
    join_one_with_deadline("drainer", drainer_done, drainer_task, deadline).await;
}

/// Await one outstanding handle until `deadline`, then `.abort()` it if it has
/// not resolved. No-ops when `already_done` (the handle resolved in the
/// `select!` and must not be awaited again).
async fn join_one_with_deadline(
    task: &str,
    already_done: bool,
    handle: tokio::task::JoinHandle<()>,
    deadline: tokio::time::Instant,
) {
    if already_done {
        return;
    }
    // Grab the abort handle BEFORE moving `handle` into `timeout_at`: a timeout
    // consumes the `JoinHandle` (dropping it), and dropping a `JoinHandle` does
    // NOT cancel the underlying `tokio::spawn`'d task — only an explicit abort
    // does. So the abort handle is load-bearing for the wedged path.
    let abort = handle.abort_handle();
    if tokio::time::timeout_at(deadline, handle).await.is_err() {
        // Grace elapsed: the task is wedged (almost certainly mid network I/O).
        // Force it down so shutdown is guaranteed to complete.
        warn!(
            task,
            grace_secs = SHUTDOWN_GRACE.as_secs(),
            "daemon task did not stop within the shutdown grace; aborting"
        );
        abort.abort();
    }
}

/// Loudly log when a daemon task's `JoinHandle` resolves *unexpectedly* — i.e.
/// before cancellation was requested. Each task body is an infinite loop that
/// must only return via its cancel arm, so a handle resolving while
/// `*cancel_rx.borrow()` is still `false` is a bug (a panic, or a stray
/// `return`): the join loop will trip global shutdown either way, but this
/// distinguishes it from the normal cancel-driven exit so it shows up in logs.
fn report_unexpected_task_exit(
    task: &str,
    res: Result<(), tokio::task::JoinError>,
    cancel_rx: &watch::Receiver<bool>,
) {
    let cancel_requested = *cancel_rx.borrow();
    match (res, cancel_requested) {
        // Panic: always loud, regardless of cancel state.
        (Err(_), _) => error!(task, "daemon task panicked; tripping shutdown"),
        // Clean return while we never asked it to stop — it exited on its own.
        (Ok(()), false) => error!(
            task,
            "daemon task returned unexpectedly (not cancel-driven); tripping shutdown"
        ),
        // Expected: a cancel-driven exit. Nothing to flag.
        (Ok(()), true) => {}
    }
}

/// Resolve when the process should stop. SIGINT (ctrl-c) everywhere; SIGTERM
/// additionally on unix, because under launchd / systemd SIGTERM is the normal
/// stop signal (the old ctrl-c-only handler never saw it). Whichever arrives
/// first wins.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal as unix_signal};
        // If installing the SIGTERM handler somehow fails, degrade to
        // ctrl-c-only rather than aborting the daemon — a daemon that still
        // stops on SIGINT is strictly better than one that refuses to start.
        let mut sigterm = match unix_signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to install SIGTERM handler; ctrl-c only");
                let _ = signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = signal::ctrl_c() => info!("SIGINT received"),
            _ = sigterm.recv() => info!("SIGTERM received"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = signal::ctrl_c().await;
        info!("ctrl-c received");
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
        PortResult, ProjectRepository, RemoteTaskCreate, RemoteTaskProvider, RemoteTaskSnapshot,
        RemoteTaskUpdate, RepoBindingRepository, TaskRepository, WorkspaceRepository,
    };
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use testing_fixtures::{
        InMemoryOutboxRepository, InMemoryProjectRepository, InMemoryRemoteProjectProvider,
        InMemoryRepoBindingRepository, InMemoryTaskRepository, InMemoryWorkspaceRepository,
        StubFilesystemProbe,
    };

    /// Poll `cond` until it returns true or `timeout` elapses, then return.
    /// Replaces fixed `sleep`-then-assert in the run-loop tests: the caller
    /// asserts the condition itself afterwards, so a never-satisfied condition
    /// still fails (fast on success, bounded on failure) rather than flaking on
    /// a too-short fixed sleep under slow CI.
    async fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + timeout;
        while !cond() {
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[derive(Default)]
    struct CountingProvider {
        updates: AtomicUsize,
    }

    #[async_trait]
    impl RemoteTaskProvider for CountingProvider {
        async fn create_remote(&self, cmd: RemoteTaskCreate<'_>) -> PortResult<RemoteTaskSnapshot> {
            Ok(RemoteTaskSnapshot {
                remote_id: "777".into(),
                node_id: Some("I_kwDOstub777".into()),
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
                node_id: None,
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
        // Persist the Promote snapshot (mirrors what `SyncService::promote`
        // does via `save_with_outbox`) so the in-memory repo's snapshot
        // history has a baseline-eligible row for the diff to anchor
        // against.
        task_repo
            .save(&task, SnapshotSource::Promote)
            .await
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
            None,
            Some(outbox_dyn),
        );

        // Stage-7 split: the poller task reconciles worktrees (tick_once); the
        // drainer task drains the outbox (drain_tick). Drive both here.
        let report = daemon.tick_once().await.unwrap();
        assert_eq!(report.workspaces, 1);
        assert_eq!(report.repos_checked, 1);
        assert_eq!(report.worktrees_checked, 2);
        assert_eq!(report.marked_missing, 1);
        assert_eq!(report.pruned, 0);

        let drained = daemon.drain_tick().await.unwrap();
        assert_eq!(drained, 1, "the one outbox entry drained");
        assert_eq!(provider.updates.load(Ordering::SeqCst), 1);

        // Second round: reconcile finds nothing newly missing; the entry is
        // succeeded so a second drain is a no-op — no duplicate remote write.
        let report = daemon.tick_once().await.unwrap();
        assert_eq!(report.marked_missing, 0);
        let drained = daemon.drain_tick().await.unwrap();
        assert_eq!(drained, 0);
        assert_eq!(
            provider.updates.load(Ordering::SeqCst),
            1,
            "no duplicate remote write on the second drain"
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
            None, // drainer
            None, // poller
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
            None, // drainer
            None, // poller
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
        let daemon = Daemon::new(workspaces, bindings, task_repo, probe, None, None, None);

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
        let daemon = Daemon::new(workspaces, bindings, task_repo, probe_dyn, None, None, None);
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
        let daemon = Daemon::new(workspaces, bindings, task_repo, probe_dyn, None, None, None)
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
        let daemon = Daemon::new(workspaces, bindings, task_repo, probe_dyn, None, None, None)
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
        let daemon = Daemon::new(workspaces, bindings, task_repo, probe, None, None, None)
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

    // ---- Stage 7: two-task run loop --------------------------------------

    /// A `RemoteProjectProvider` that counts `poll_project_items` calls (the
    /// poller task's network touch) and no-ops every mutation. Lets the run
    /// loop tests observe "the poller actually polled".
    #[derive(Default)]
    struct CountingProjectProvider {
        polls: AtomicUsize,
    }

    #[async_trait]
    impl ports::RemoteProjectProvider for CountingProjectProvider {
        async fn fetch_project(&self, _: &str, _: u64) -> PortResult<ports::RemoteProjectSnapshot> {
            Err(ports::PortError::NotFound("n/a".into()))
        }
        async fn add_item(&self, _: &str, _: &str) -> PortResult<String> {
            Ok("PVTI_x".into())
        }
        async fn create_draft_issue(&self, _: &str, _: &str, _: &str) -> PortResult<String> {
            Ok("PVTI_x".into())
        }
        async fn update_draft_issue(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> PortResult<()> {
            Ok(())
        }
        async fn convert_draft_to_issue(&self, _: &str, _: &str) -> PortResult<(String, u64)> {
            Ok(("I_x".into(), 1))
        }
        async fn set_status(&self, _: &str, _: &str, _: &str, _: &str) -> PortResult<()> {
            Ok(())
        }
        async fn poll_project_items(
            &self,
            _: &str,
            _: &str,
            _: domain_core::Timestamp,
            _: &str,
        ) -> PortResult<ports::PollPage> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            Ok(ports::PollPage {
                items: Vec::new(),
                truncated: false,
            })
        }
    }

    /// A `RemoteProjectProvider` that panics on its first poll — used to prove
    /// a panic in the poller task trips the shared cancellation and `run`
    /// returns (taking the drainer task down with it).
    struct PanicProjectProvider;

    #[async_trait]
    impl ports::RemoteProjectProvider for PanicProjectProvider {
        async fn fetch_project(&self, _: &str, _: u64) -> PortResult<ports::RemoteProjectSnapshot> {
            Err(ports::PortError::NotFound("n/a".into()))
        }
        async fn add_item(&self, _: &str, _: &str) -> PortResult<String> {
            Ok("PVTI_x".into())
        }
        async fn create_draft_issue(&self, _: &str, _: &str, _: &str) -> PortResult<String> {
            Ok("PVTI_x".into())
        }
        async fn update_draft_issue(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> PortResult<()> {
            Ok(())
        }
        async fn convert_draft_to_issue(&self, _: &str, _: &str) -> PortResult<(String, u64)> {
            Ok(("I_x".into(), 1))
        }
        async fn set_status(&self, _: &str, _: &str, _: &str, _: &str) -> PortResult<()> {
            Ok(())
        }
        async fn poll_project_items(
            &self,
            _: &str,
            _: &str,
            _: domain_core::Timestamp,
            _: &str,
        ) -> PortResult<ports::PollPage> {
            panic!("boom: poller task panic injection");
        }
    }

    /// A `RemoteProjectProvider` whose `poll_project_items` never resolves —
    /// it parks forever (simulating a wedged network I/O call). The poller
    /// task's first immediate tick enters it and stays there, so the task body
    /// never reaches its `cancel` arm. Used to prove the shutdown-grace abort
    /// fallback force-stops a task that ignores cancellation.
    struct HangingProjectProvider;

    #[async_trait]
    impl ports::RemoteProjectProvider for HangingProjectProvider {
        async fn fetch_project(&self, _: &str, _: u64) -> PortResult<ports::RemoteProjectSnapshot> {
            Err(ports::PortError::NotFound("n/a".into()))
        }
        async fn add_item(&self, _: &str, _: &str) -> PortResult<String> {
            Ok("PVTI_x".into())
        }
        async fn create_draft_issue(&self, _: &str, _: &str, _: &str) -> PortResult<String> {
            Ok("PVTI_x".into())
        }
        async fn update_draft_issue(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> PortResult<()> {
            Ok(())
        }
        async fn convert_draft_to_issue(&self, _: &str, _: &str) -> PortResult<(String, u64)> {
            Ok(("I_x".into(), 1))
        }
        async fn set_status(&self, _: &str, _: &str, _: &str, _: &str) -> PortResult<()> {
            Ok(())
        }
        async fn poll_project_items(
            &self,
            _: &str,
            _: &str,
            _: domain_core::Timestamp,
            _: &str,
        ) -> PortResult<ports::PollPage> {
            // Park forever — ignores cancellation, just like a stalled I/O call.
            std::future::pending::<()>().await;
            unreachable!("hanging provider never resolves")
        }
    }

    /// A minimal project for the poller to enumerate. The caller persists it
    /// (`save` is async).
    fn seed_project() -> domain_project::Project {
        domain_project::Project::new(
            domain_core::ProjectId::parse("PVT_kwHO_run").unwrap(),
            "acme".into(),
            1,
            "Board".into(),
            "PVTSSF_field".into(),
            vec![],
            vec![],
            false,
            domain_core::Timestamp::now(),
        )
        .unwrap()
    }

    /// #88 regression: both tasks do their FIRST unit of work IMMEDIATELY on
    /// startup, not after a full interval. We set the poller interval to an
    /// hour; if the first poll were delayed a full interval the poll count
    /// would still be 0 after a short wait. We assert it polled (and the
    /// drainer drained its seeded entry) well before the interval elapses,
    /// then trigger a clean shutdown.
    #[tokio::test]
    async fn both_tasks_do_first_work_immediately() {
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let bind_repo = Arc::new(InMemoryRepoBindingRepository::new());
        let task_repo = Arc::new(InMemoryTaskRepository::new());
        let proj_repo = Arc::new(InMemoryProjectRepository::new());
        let outbox = Arc::new(InMemoryOutboxRepository::new());
        let provider = Arc::new(CountingProvider::default());
        let proj_provider = Arc::new(CountingProjectProvider::default());

        // A project for the poller to poll on its first tick.
        let project = seed_project();
        proj_repo.save(&project).await.unwrap();

        // A workspace + a dirty issue-backed task with a seeded outbox entry so
        // the drainer's first drain calls the provider.
        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        ws_repo.save(&ws).await.unwrap();
        let mut task = Task::new_draft(ws.id, None, "edit".into()).unwrap();
        task.stage_for_sync().unwrap();
        task.promote_to_remote(domain_task::RemoteRef::new("github", "1"))
            .unwrap();
        // Persist the Promote snapshot so the in-memory repo's history
        // has a baseline-eligible row (mirrors what `SyncService::promote`
        // does).
        task_repo
            .save(&task, SnapshotSource::Promote)
            .await
            .unwrap();
        // Then a body edit so the diff is non-empty (the helper
        // short-circuits empty patches).
        task.set_body("edit body".into());
        task_repo
            .save(&task, SnapshotSource::LocalEdit)
            .await
            .unwrap();
        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "1".into(),
                title: None,
                body: None,
                closed: None,
            },
        );
        outbox.enqueue(&entry).await.unwrap();

        let workspaces = WorkspaceService::new(ws_repo.clone());
        let bindings = RepoBindingService::new(ws_repo.clone(), bind_repo.clone());
        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let remote_tasks: Arc<dyn RemoteTaskProvider> = provider.clone();
        let remote_projects: Arc<dyn ports::RemoteProjectProvider> = proj_provider.clone();
        let drainer = Arc::new(OutboxDrainer::new(
            outbox_dyn.clone(),
            task_repo.clone(),
            ws_repo.clone(),
            proj_repo.clone(),
            remote_tasks,
            remote_projects.clone(),
        ));
        let poller = Arc::new(ProjectPoller::new(
            proj_repo.clone(),
            task_repo.clone(),
            remote_projects,
        ));
        let daemon = Arc::new(Daemon::new(
            workspaces,
            bindings,
            task_repo,
            Arc::new(StubFilesystemProbe::new()),
            Some(drainer),
            Some(poller),
            Some(outbox_dyn),
        ));

        // Long interval: if first work waited a full interval, nothing happens.
        let shutdown = Arc::new(Notify::new());
        let run = {
            let daemon = daemon.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                daemon
                    .run_until(Duration::from_secs(3600), async move {
                        shutdown.notified().await
                    })
                    .await
            })
        };

        // Poll for the immediate first work rather than sleeping a fixed beat
        // (a fixed sleep is flaky on slow CI). Both counters must go non-zero
        // well before the 1h interval — proving first-work-is-immediate (#88) —
        // so a generous timeout still fails fast if the work never happens.
        wait_until(Duration::from_secs(5), || {
            proj_provider.polls.load(Ordering::SeqCst) >= 1
                && provider.updates.load(Ordering::SeqCst) >= 1
        })
        .await;
        assert!(
            proj_provider.polls.load(Ordering::SeqCst) >= 1,
            "poller must poll immediately, not after the 1h interval (#88)"
        );
        assert!(
            provider.updates.load(Ordering::SeqCst) >= 1,
            "drainer must drain immediately, not after a sweep delay (#88)"
        );

        // Clean shutdown stops both tasks and `run` returns Ok.
        shutdown.notify_one();
        let res = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("run did not return after shutdown");
        assert!(res.unwrap().is_ok(), "clean shutdown returns Ok");
    }

    /// A panic in one spawned task trips the shared cancellation so the other
    /// task stops too and `run` returns.
    #[tokio::test]
    async fn panic_in_one_task_trips_shutdown_and_run_returns() {
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let bind_repo = Arc::new(InMemoryRepoBindingRepository::new());
        let task_repo = Arc::new(InMemoryTaskRepository::new());
        let proj_repo = Arc::new(InMemoryProjectRepository::new());
        let outbox = Arc::new(InMemoryOutboxRepository::new());

        // A project so the panicking provider's poll is actually invoked.
        let project = seed_project();
        proj_repo.save(&project).await.unwrap();
        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        ws_repo.save(&ws).await.unwrap();

        let workspaces = WorkspaceService::new(ws_repo.clone());
        let bindings = RepoBindingService::new(ws_repo.clone(), bind_repo.clone());
        let remote_projects: Arc<dyn ports::RemoteProjectProvider> = Arc::new(PanicProjectProvider);
        let poller = Arc::new(ProjectPoller::new(
            proj_repo.clone(),
            task_repo.clone(),
            remote_projects,
        ));
        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let daemon = Arc::new(Daemon::new(
            workspaces,
            bindings,
            task_repo,
            Arc::new(StubFilesystemProbe::new()),
            None,
            Some(poller),
            Some(outbox_dyn),
        ));

        // No shutdown signal is sent: run must return on its own because the
        // poller task panics on its first immediate tick, which trips the
        // shared cancellation and stops the drainer task too.
        let never = std::future::pending::<()>();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            daemon.run_until(Duration::from_secs(3600), never),
        )
        .await
        .expect("run did not return after a task panicked");
        assert!(
            res.is_ok(),
            "run returns Ok after a task panic trips shutdown"
        );
    }

    /// Shutdown-grace abort fallback (#55, r3325417131): a poller task wedged in
    /// I/O that never observes cancellation is force-`.abort()`ed within
    /// `SHUTDOWN_GRACE`, so `run_until` always returns rather than hanging on the
    /// post-cancel join. The hanging provider parks the poller's first immediate
    /// tick inside `tick_once`, so cancellation can't preempt it; only the
    /// bounded join + abort can complete shutdown. Paused time advances the 5s
    /// grace virtually, keeping the test fast and deterministic.
    #[tokio::test(start_paused = true)]
    async fn wedged_task_is_force_aborted_within_shutdown_grace() {
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let bind_repo = Arc::new(InMemoryRepoBindingRepository::new());
        let task_repo = Arc::new(InMemoryTaskRepository::new());
        let proj_repo = Arc::new(InMemoryProjectRepository::new());
        let outbox = Arc::new(InMemoryOutboxRepository::new());

        // A project so the hanging provider's poll is actually invoked and parks.
        let project = seed_project();
        proj_repo.save(&project).await.unwrap();
        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        ws_repo.save(&ws).await.unwrap();

        let workspaces = WorkspaceService::new(ws_repo.clone());
        let bindings = RepoBindingService::new(ws_repo.clone(), bind_repo.clone());
        let remote_projects: Arc<dyn ports::RemoteProjectProvider> =
            Arc::new(HangingProjectProvider);
        let poller = Arc::new(ProjectPoller::new(
            proj_repo.clone(),
            task_repo.clone(),
            remote_projects,
        ));
        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let daemon = Arc::new(Daemon::new(
            workspaces,
            bindings,
            task_repo,
            Arc::new(StubFilesystemProbe::new()),
            None,
            Some(poller),
            Some(outbox_dyn),
        ));

        // Shut down almost immediately; the poller task is parked inside its
        // hanging first poll and will never reach its cancel arm, so only the
        // grace-bounded join + abort can let `run_until` return. The outer
        // bound is comfortably larger than `SHUTDOWN_GRACE` (5s) but far smaller
        // than the 1h poller interval — under paused time it advances virtually.
        let shutdown = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let res = tokio::time::timeout(
            Duration::from_secs(30),
            daemon.run_until(Duration::from_secs(3600), shutdown),
        )
        .await
        .expect("run_until must return within the grace via the abort fallback");
        assert!(
            res.is_ok(),
            "run_until returns Ok once the wedged task is aborted"
        );
    }

    /// With `github_token = None` both poller and drainer are `None`: the run
    /// loop still spins (reconcile + heartbeat) and shuts down cleanly without
    /// panicking.
    #[tokio::test]
    async fn no_token_poller_and_drainer_noop_without_panic() {
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let bind_repo = Arc::new(InMemoryRepoBindingRepository::new());
        let task_repo = Arc::new(InMemoryTaskRepository::new());

        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        ws_repo.save(&ws).await.unwrap();

        let workspaces = WorkspaceService::new(ws_repo.clone());
        let bindings = RepoBindingService::new(ws_repo.clone(), bind_repo.clone());
        let tmp = tempfile::TempDir::new().unwrap();
        let daemon = Arc::new(
            Daemon::new(
                workspaces,
                bindings,
                task_repo,
                Arc::new(StubFilesystemProbe::new()),
                None,
                None,
                None,
            )
            .with_state_dir(tmp.path().to_path_buf())
            .with_interval_secs(1),
        );

        let shutdown = Arc::new(Notify::new());
        let run = {
            let daemon = daemon.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                daemon
                    .run_until(Duration::from_secs(3600), async move {
                        shutdown.notified().await
                    })
                    .await
            })
        };

        // Poll for the heartbeat (the poller task's first immediate tick writes
        // last_tick.json) instead of sleeping a fixed beat, which is flaky on
        // slow CI. Once it lands we shut down and assert it's parseable under
        // the new two-task structure (#55 keeps one primary cadence) — and that
        // nothing panicked.
        let path = tmp.path().join("last_tick.json");
        wait_until(Duration::from_secs(5), || path.exists()).await;
        shutdown.notify_one();
        let res = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("run did not return after shutdown");
        assert!(res.unwrap().is_ok());

        assert!(path.exists(), "heartbeat still written with no token");
        let parsed: LastTick =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.report.workspaces, 1);
    }
}
