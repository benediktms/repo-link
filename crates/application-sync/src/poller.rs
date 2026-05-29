//! [`ProjectPoller`] — the inbound polling path for project-backed mirror
//! tasks (RFC 0001 Stage 7, §D4, #55).
//!
//! # Model
//!
//! Where the [`OutboxDrainer`](crate::OutboxDrainer) pushes *local* edits to
//! GitHub, the poller pulls *remote* project state back. Once per cadence it
//! enumerates every locally-known project and asks the
//! [`RemoteProjectProvider`] for the items that changed since the last poll
//! (per §D4 the delta lever is `updated:>{since}` alone — see [`POLL_QUERY`];
//! the older `is:open` filter wrongly dropped drafts and closed/Done items).
//! Each returned item is correlated with its local task by
//! `project_item_id`; what is reconcilable *now* is reconciled.
//!
//! # Project-status cache (Stage 8, closes #39)
//!
//! Each correlated item's `status_option_id` is written into the local task's
//! cached `project_status_option_id` column. That cache is the remote-board
//! status `rl query drift` compares — *independently* of `sync_state` —
//! against the option the task's local lifecycle status maps to.
//!
//! The cache is persisted via a **targeted single-column update**
//! ([`TaskRepository::cache_project_status`]), never the whole-row `save`
//! aggregate path (#56, CodeRabbit thread r3325841752). The poller snapshots
//! every task once per pass; routing the cache write through `save` would
//! re-emit that stale snapshot and clobber any title / body / status /
//! `sync_state` edit a concurrent CLI made after the snapshot — a data-loss
//! race for what is only a write-through cache column. The targeted update
//! touches that one column and nothing else, so it can't tear newer fields,
//! writes no snapshot, and never changes `sync_state` (a board move is a
//! separate drift axis, not a `SyncState` transition). The write is
//! idempotent: the cache is only persisted when the polled value actually
//! differs from what's indexed, so a steady-state re-poll does no writes.
//!
//! # Truncation / partiality
//!
//! The provider paginates GitHub's `project.items(first: …)`. GitHub caps a
//! single connection traversal, so the provider reports a truthful
//! [`PollPage::truncated`](ports::PollPage) flag when it could not enumerate
//! the whole result set. On a truncated page the poll watermark is *not*
//! advanced for that project, so the next cycle refetches the same window
//! rather than skipping the unseen tail. The flag is authoritative: we never
//! infer partiality from the item count, because the provider silently drops
//! unmodelled nodes (PRs, hidden content) and a truncated page can therefore
//! carry fewer items than a naive count heuristic would expect. Crucially we
//! never infer "an item we didn't see is gone/stale" from a poll: absence in a
//! (possibly truncated) page is not evidence of remote deletion.

use std::collections::HashMap;
use std::sync::Arc;

use domain_core::{ProjectId, Timestamp};
use domain_task::Task;
#[cfg(doc)]
use ports::PollPage;
use ports::{
    ProjectRepository, RemoteProjectItem, RemoteProjectProvider, TaskFilter, TaskRepository,
};
use tracing::{Instrument, debug, info, info_span, warn};

use crate::error::Result;

/// The GitHub `project.items(query:)` filter the poller sends every cycle
/// (RFC §D4). Empty on purpose: the graphql layer composes it into just
/// `updated:>{since}`, so the per-project `since` watermark is the *only*
/// delta lever and the page already stays proportional to the change rate.
///
/// It used to be `"is:open"`, but that was an over-restriction (issue
/// r3325531902): GitHub Projects `is:open` matches open issues/PRs only, so it
/// silently dropped **draft items** (draft-backed mirror tasks were never
/// polled or reconciled) and **items that moved to a closed/Done state** — the
/// exact status transitions a reconciliation poller must observe. Space-joining
/// `is:open is:draft` would AND (match nothing), so the fix is to drop the
/// `is:` filter entirely and lean on `updated:>{since}` alone.
const POLL_QUERY: &str = "";

/// Outcome of one [`ProjectPoller::poll_once`] pass. Returned so the daemon
/// loop can log progress and tests can assert reconcile counts.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PollReport {
    /// Projects enumerated and polled this pass.
    pub projects_polled: usize,
    /// Items returned across all projects.
    pub items_seen: usize,
    /// Items correlated to a local task by `project_item_id`.
    pub items_matched: usize,
    /// Items with no local task (logged + skipped — never a panic).
    pub items_unmatched: usize,
    /// Projects whose page came back `truncated` (the provider could not
    /// enumerate the whole set) and were therefore treated as partial
    /// (watermark not advanced).
    pub partial_projects: usize,
}

/// Polls every known project for changed items and reconciles them against
/// the local task cache. Holds only the handles it needs; the per-project
/// `since` watermark lives in-process (see [`Self::poll_once`]).
pub struct ProjectPoller {
    projects: Arc<dyn ProjectRepository>,
    tasks: Arc<dyn TaskRepository>,
    remote_projects: Arc<dyn RemoteProjectProvider>,
    /// Per-project poll watermark (`since`). Process-local: a daemon restart
    /// forgets it and re-polls from epoch once, which is the safe direction
    /// (re-reading is idempotent; the reconcile is a no-op when nothing
    /// changed). `std::sync` — never held across an `.await`.
    watermarks: std::sync::Mutex<HashMap<ProjectId, Timestamp>>,
}

impl ProjectPoller {
    pub fn new(
        projects: Arc<dyn ProjectRepository>,
        tasks: Arc<dyn TaskRepository>,
        remote_projects: Arc<dyn RemoteProjectProvider>,
    ) -> Self {
        Self {
            projects,
            tasks,
            remote_projects,
            watermarks: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// One full poll + reconcile pass across every known project. Per-project
    /// failures are logged and skipped (a flaky provider for one board must
    /// not abort the others); the pass returns `Err` only on a local
    /// repository failure that can't be attributed to a single project.
    pub async fn poll_once(&self) -> Result<PollReport> {
        let mut report = PollReport::default();

        let projects = self.projects.list_all().await?;
        if projects.is_empty() {
            debug!("poll: no projects to poll");
            return Ok(report);
        }

        // One snapshot of the local tasks per pass, indexed by their
        // `project_item_id`. Cheaper than a per-item repository round-trip and
        // there's no project_item_id filter on `TaskFilter`. `include_archived`
        // so a polled item whose local task is archived still correlates (so
        // the cache write isn't skipped for it) rather than appearing
        // unmatched. Held `mut` so the cache write updates the in-memory copy
        // too — a duplicate item id later in the same pass then compares
        // against the just-written value and stays idempotent.
        let mut by_item_id = self.index_tasks_by_item_id().await?;

        for project in &projects {
            report.projects_polled += 1;
            let project_id = project.id.clone();
            let since = self.watermark(&project_id);

            // Per-project span so the reconcile events below nest under it.
            let res = async {
                self.remote_projects
                    .poll_project_items(
                        project.id.as_str(),
                        &project.status_field_id,
                        since,
                        POLL_QUERY,
                    )
                    .await
            }
            .instrument(info_span!(
                "project_poll",
                project = %project.id.as_str(),
                title = %project.title
            ))
            .await;

            let page = match res {
                Ok(page) => page,
                Err(e) => {
                    // One board's provider hiccup must not sink the others.
                    warn!(project = %project.id.as_str(), error = %e, "project poll failed; skipping this project this cycle");
                    continue;
                }
            };

            report.items_seen += page.items.len();

            // Watermark = newest item updated_at this page. Advance only on a
            // complete read: a truncated page would skip the unseen tail. The
            // provider's `truncated` flag is authoritative — we never re-derive
            // partiality from the item count (unmodelled nodes are dropped, so
            // a truncated page can carry fewer items than a count heuristic
            // expects, and would be mistaken for complete).
            let mut max_seen = since;

            for item in &page.items {
                if item.updated_at.into_inner() > max_seen.into_inner() {
                    max_seen = item.updated_at;
                }
                self.reconcile_item(&mut by_item_id, item, &mut report)
                    .await;
            }

            if page.truncated {
                report.partial_projects += 1;
                warn!(
                    project = %project.id.as_str(),
                    items = page.items.len(),
                    "poll page truncated; watermark not advanced, will refetch next cycle"
                );
                // Deliberately do NOT advance the watermark or mark anything
                // stale — absence from a truncated page is not deletion.
            } else if max_seen > since {
                // Advance only on a strictly-newer item. The 1s safety margin
                // (M-p1) pulls the watermark back one second before the next
                // strict `updated:>` query: GitHub's `updated:>` is
                // strict-greater, so advancing to exactly `max_seen` would drop
                // any sibling that shares the same second as the newest item we
                // saw. Re-reading is idempotent (Stage 7 reconcile is inert;
                // Stage 8's cache write will be idempotent), so over-fetching one
                // second is strictly safer than under-fetching same-second
                // deltas. `max(since, …)` clamps the margin so it can never move
                // the watermark *below* the current `since` — a complete page
                // whose newest item is only one second past `since` keeps
                // `since`, never regresses. (Without the `max_seen > since`
                // guard, an empty / nothing-newer page would set the watermark
                // to `since - 1s` and drift `since` backward 1s every poll,
                // re-widening the fetch window each cycle.)
                self.set_watermark(project_id, since.max(max_seen.minus_one_second()));
            }
            // else: complete page with nothing strictly newer than `since`
            // (`max_seen == since`) — leave the watermark unchanged so it never
            // drifts backward.
        }

        info!(
            projects = report.projects_polled,
            items = report.items_seen,
            matched = report.items_matched,
            unmatched = report.items_unmatched,
            partial = report.partial_projects,
            "project poll complete"
        );
        Ok(report)
    }

    /// Correlate one polled item with a local task by `project_item_id` and
    /// reconcile its cached project-board status (RFC 0001 Stage 8, closes
    /// #39). Writes `item.status_option_id` into the matched task's
    /// `project_status_option_id` cache — the value `rl query drift` compares
    /// against the task's mapped local status, independently of `sync_state`.
    ///
    /// The cache is persisted via a **targeted single-column update**
    /// ([`TaskRepository::cache_project_status`]), never the whole-row `save`
    /// aggregate path (#56, thread r3325841752). The per-pass task index is a
    /// stale snapshot; saving the full row would clobber any concurrent CLI
    /// edit to title / body / status / `sync_state` made after the snapshot.
    /// The targeted update can't tear those fields, writes no snapshot, and
    /// never changes `sync_state`.
    ///
    /// Idempotent: persists only when the polled value differs from the
    /// indexed task's cached value, so a steady-state re-poll does no writes. A
    /// persist failure for one item is logged and skipped — never a panic and
    /// never an abort of the rest of the page (the cache is a hint; the next
    /// poll re-attempts). Unmatched items are logged and skipped.
    async fn reconcile_item(
        &self,
        by_item_id: &mut HashMap<String, Task>,
        item: &RemoteProjectItem,
        report: &mut PollReport,
    ) {
        let Some(task) = by_item_id.get_mut(&item.item_node_id) else {
            report.items_unmatched += 1;
            debug!(
                item = %item.item_node_id,
                "polled project item has no local task; skipping (may be a board item we don't mirror)"
            );
            return;
        };
        report.items_matched += 1;

        // Cheap no-op skip: the polled value already matches what's cached on
        // the indexed task, so steady-state re-polls do no writes at all.
        if task.project_status_option_id == item.status_option_id {
            return;
        }

        // Persist the cache via a targeted single-column update — NOT the
        // whole-row `save`. The per-pass index is a stale snapshot; a full-row
        // write would clobber a concurrent CLI edit to other columns. The
        // targeted write touches only `project_status_option_id`, leaves
        // `sync_state` untouched, and appends no snapshot.
        if let Err(e) = self
            .tasks
            .cache_project_status(task.id, item.status_option_id.clone())
            .await
        {
            warn!(
                item = %item.item_node_id,
                task = %task.id,
                error = %e,
                "failed to persist polled project-status cache; will retry next cycle"
            );
        } else {
            // Update the in-memory index copy so a duplicate item id later in
            // the same pass compares against the just-written value and stays
            // idempotent. Per-pass scratch only — never read back as the
            // aggregate.
            task.project_status_option_id = item.status_option_id.clone();
            debug!(
                item = %item.item_node_id,
                task = %task.id,
                status_option = ?item.status_option_id,
                "cached polled project status via targeted column update"
            );
        }
    }

    /// Snapshot of every non-`None`-`project_item_id` task, keyed by item id.
    /// A duplicate item id (shouldn't happen — item ids are unique per board)
    /// keeps the last writer; logged at debug so it's diagnosable.
    ///
    // TODO(scale): this lists ALL tasks (unscoped, `O(all tasks)`) and filters
    // in memory each poll because `TaskFilter` has no project-item predicate.
    // The fix is a `TaskFilter` predicate (`has_project_item_id` / `project_id`)
    // so the repository returns only the project-backed rows we correlate.
    async fn index_tasks_by_item_id(&self) -> Result<HashMap<String, Task>> {
        let tasks = self
            .tasks
            .list(TaskFilter {
                include_archived: true,
                ..TaskFilter::default()
            })
            .await?;
        let mut by_item_id = HashMap::new();
        for t in tasks {
            if let Some(item_id) = t.project_item_id.clone()
                && by_item_id.insert(item_id.clone(), t).is_some()
            {
                debug!(item = %item_id, "two local tasks share a project_item_id; keeping the last");
            }
        }
        Ok(by_item_id)
    }

    fn watermark(&self, project_id: &ProjectId) -> Timestamp {
        self.watermarks
            .lock()
            .unwrap()
            .get(project_id)
            .copied()
            .unwrap_or_else(Timestamp::epoch)
    }

    fn set_watermark(&self, project_id: ProjectId, ts: Timestamp) {
        self.watermarks.lock().unwrap().insert(project_id, ts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_core::WorkspaceId;
    use domain_project::Project;
    use domain_task::{RemoteRef, SnapshotSource, SyncState, Task};
    use ports::ProjectRepository;
    use testing_fixtures::{
        InMemoryProjectRepository, InMemoryRemoteProjectProvider, InMemoryTaskRepository,
        ProjectCall,
    };

    fn project(node_id: &str) -> Project {
        Project::new(
            ProjectId::parse(node_id).unwrap(),
            "acme".into(),
            1,
            "Board".into(),
            "PVTSSF_field".into(),
            vec![],
            vec![],
            false,
            Timestamp::now(),
        )
        .unwrap()
    }

    fn item(item_node_id: &str, status_option_id: Option<&str>) -> RemoteProjectItem {
        RemoteProjectItem {
            item_node_id: item_node_id.into(),
            issue_node_id: Some("I_1".into()),
            canonical_repo: Some("github.com/o/r".into()),
            number: Some(1),
            title: "polled".into(),
            body: "body".into(),
            closed: false,
            status_option_id: status_option_id.map(str::to_owned),
            updated_at: Timestamp::now(),
        }
    }

    async fn poller(
        projects: Arc<InMemoryProjectRepository>,
        tasks: Arc<InMemoryTaskRepository>,
        remote: Arc<InMemoryRemoteProjectProvider>,
    ) -> ProjectPoller {
        let p: Arc<dyn ProjectRepository> = projects;
        let t: Arc<dyn TaskRepository> = tasks;
        let r: Arc<dyn RemoteProjectProvider> = remote;
        ProjectPoller::new(p, t, r)
    }

    /// A polled item whose `item_node_id` matches a local task's
    /// `project_item_id` is correlated (matched, not unmatched), and the
    /// project is polled with the §D4 empty query (delta-only — see
    /// [`POLL_QUERY`]) against its own status_field_id.
    #[tokio::test]
    async fn poll_once_reconciles_a_matched_item() {
        let projects = Arc::new(InMemoryProjectRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());
        let remote = Arc::new(InMemoryRemoteProjectProvider::new());

        let proj = project("PVT_kwHO_match");
        projects.save(&proj).await.unwrap();

        // A local mirror task already carrying the project item id.
        let ws = WorkspaceId::new();
        let mut task = Task::new_draft(ws, None, "mirror".into()).unwrap();
        task.stage_for_sync().unwrap();
        task.promote_to_remote(RemoteRef::new("github", "1"))
            .unwrap();
        task.project_item_id = Some("PVTI_42".into());
        tasks.save(&task, SnapshotSource::Promote).await.unwrap();

        remote.set_poll_items("PVT_kwHO_match", vec![item("PVTI_42", Some("o_wip"))]);

        let poller = poller(projects, tasks, remote.clone()).await;
        let report = poller.poll_once().await.unwrap();

        assert_eq!(report.projects_polled, 1);
        assert_eq!(report.items_seen, 1);
        assert_eq!(report.items_matched, 1);
        assert_eq!(report.items_unmatched, 0);
        assert_eq!(report.partial_projects, 0);

        // Polled with the right project + field + the §D4 delta-only query
        // (empty, so the graphql layer sends just `updated:>{since}`).
        let calls = remote.calls();
        assert!(calls.iter().any(|c| matches!(
            c,
            ProjectCall::Poll { project_node_id, status_field_id, query }
                if project_node_id == "PVT_kwHO_match"
                    && status_field_id == "PVTSSF_field"
                    && query.is_empty()
        )));
    }

    /// A polled item with no local task is skipped (counted unmatched), no
    /// panic, and the matched item in the same page still reconciles.
    #[tokio::test]
    async fn poll_once_skips_item_with_no_local_task() {
        let projects = Arc::new(InMemoryProjectRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());
        let remote = Arc::new(InMemoryRemoteProjectProvider::new());

        let proj = project("PVT_kwHO_skip");
        projects.save(&proj).await.unwrap();

        let ws = WorkspaceId::new();
        let mut task = Task::new_draft(ws, None, "mirror".into()).unwrap();
        task.project_item_id = Some("PVTI_known".into());
        tasks.save(&task, SnapshotSource::LocalEdit).await.unwrap();

        remote.set_poll_items(
            "PVT_kwHO_skip",
            vec![item("PVTI_known", None), item("PVTI_orphan", None)],
        );

        let poller = poller(projects, tasks, remote).await;
        let report = poller.poll_once().await.unwrap();

        assert_eq!(report.items_seen, 2);
        assert_eq!(report.items_matched, 1, "the known item correlated");
        assert_eq!(report.items_unmatched, 1, "the orphan item was skipped");
    }

    /// A page the provider flags `truncated` is treated as partial: the
    /// project is counted partial and the watermark is NOT advanced, so the
    /// next cycle re-polls the same `since` window (epoch here). The flag is
    /// driven directly via the stub — partiality is the provider's truthful
    /// signal, not a count heuristic, so even a one-item page is partial when
    /// flagged truncated.
    #[tokio::test]
    async fn poll_once_treats_truncated_page_as_partial() {
        let projects = Arc::new(InMemoryProjectRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());
        let remote = Arc::new(InMemoryRemoteProjectProvider::new());

        let proj = project("PVT_kwHO_trunc");
        projects.save(&proj).await.unwrap();

        // A single unmatched item — but the provider reports the read as
        // truncated. A count heuristic would call this "complete"; the
        // authoritative flag must still make it partial.
        remote.set_poll_items("PVT_kwHO_trunc", vec![item("PVTI_0", None)]);
        remote.set_poll_truncated("PVT_kwHO_trunc", true);

        let poller = poller(projects, tasks, remote).await;
        let report = poller.poll_once().await.unwrap();

        assert_eq!(report.partial_projects, 1, "truncated flag → partial");
        assert_eq!(report.items_seen, 1);
        // Watermark stays at epoch (not advanced) so the next cycle refetches.
        let wm = poller.watermark(&ProjectId::parse("PVT_kwHO_trunc").unwrap());
        assert_eq!(
            wm.into_inner(),
            Timestamp::epoch().into_inner(),
            "watermark must not advance on a truncated read"
        );
    }

    /// On a complete (non-truncated) page the watermark advances to
    /// `max_seen - 1s`, not exactly `max_seen`. The 1s margin re-includes
    /// same-second siblings on the next strict `updated:>` query (M-p1).
    #[tokio::test]
    async fn poll_once_advances_watermark_with_one_second_margin() {
        let projects = Arc::new(InMemoryProjectRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());
        let remote = Arc::new(InMemoryRemoteProjectProvider::new());

        let proj = project("PVT_kwHO_margin");
        projects.save(&proj).await.unwrap();

        // One item with a known updated_at; the page is complete (not truncated).
        let seen_at = Timestamp::now();
        let mut it = item("PVTI_m", None);
        it.updated_at = seen_at;
        remote.set_poll_items("PVT_kwHO_margin", vec![it]);

        let poller = poller(projects, tasks, remote).await;
        let report = poller.poll_once().await.unwrap();
        assert_eq!(report.partial_projects, 0, "complete page → not partial");

        let wm = poller.watermark(&ProjectId::parse("PVT_kwHO_margin").unwrap());
        assert_eq!(
            wm.into_inner(),
            seen_at.minus_one_second().into_inner(),
            "watermark advances to max_seen - 1s so same-second siblings re-include"
        );
        assert!(
            wm.into_inner() < seen_at.into_inner(),
            "the advanced watermark is strictly before the newest item seen"
        );
    }

    /// Regression (M-p1 follow-up): a complete page that surfaces nothing
    /// strictly newer than the current `since` must leave the watermark
    /// unchanged. Before the `max_seen > since` guard, the 1s margin was applied
    /// unconditionally, so `max_seen == since` set the watermark to
    /// `since - 1s` and drifted `since` backward one second every poll — an
    /// ever-widening re-fetch window. Here we pre-seed a non-epoch watermark,
    /// poll a complete empty page (nothing seen, so `max_seen == since`), and
    /// assert the watermark did not move.
    #[tokio::test]
    async fn poll_once_empty_complete_page_does_not_regress_watermark() {
        let projects = Arc::new(InMemoryProjectRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());
        let remote = Arc::new(InMemoryRemoteProjectProvider::new());

        let proj = project("PVT_kwHO_noregress");
        projects.save(&proj).await.unwrap();

        // Complete page, no items: `max_seen` stays at `since`.
        remote.set_poll_items("PVT_kwHO_noregress", vec![]);

        let poller = poller(projects, tasks, remote).await;
        let pid = ProjectId::parse("PVT_kwHO_noregress").unwrap();

        // Pre-seed a non-epoch watermark so a backward drift would be visible.
        let seeded = Timestamp::now();
        poller.set_watermark(pid.clone(), seeded);

        let report = poller.poll_once().await.unwrap();
        assert_eq!(report.partial_projects, 0, "complete page → not partial");
        assert_eq!(report.items_seen, 0);

        let wm = poller.watermark(&pid);
        assert_eq!(
            wm.into_inner(),
            seeded.into_inner(),
            "an empty/complete page (nothing newer than `since`) must leave the watermark unchanged — no 1s backward drift"
        );
    }

    /// A complete page whose newest item is exactly one second past the current
    /// `since` advances with the margin but never *below* `since`: the
    /// `since.max(max_seen - 1s)` clamp keeps the watermark at `since` rather
    /// than regressing to `since - 1s`.
    #[tokio::test]
    async fn poll_once_advance_is_clamped_to_not_drop_below_since() {
        let projects = Arc::new(InMemoryProjectRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());
        let remote = Arc::new(InMemoryRemoteProjectProvider::new());

        let proj = project("PVT_kwHO_clamp");
        projects.save(&proj).await.unwrap();

        // The newest item is exactly `since + 1s`, so the raw margin would land
        // at `since` — the clamp keeps it there (never below).
        let since = Timestamp::now();
        let seen_at = Timestamp::from_utc(since.into_inner() + chrono::Duration::seconds(1));
        let mut it = item("PVTI_c", None);
        it.updated_at = seen_at;
        remote.set_poll_items("PVT_kwHO_clamp", vec![it]);

        let poller = poller(projects, tasks, remote).await;
        let pid = ProjectId::parse("PVT_kwHO_clamp").unwrap();
        poller.set_watermark(pid.clone(), since);

        poller.poll_once().await.unwrap();

        let wm = poller.watermark(&pid);
        assert_eq!(
            wm.into_inner(),
            since.into_inner(),
            "advance clamps to `since`; the 1s margin must never move the watermark below the prior `since`"
        );
    }

    /// Stage 8 (#39, #56): a matched item's `status_option_id` is written into
    /// the local task's `project_status_option_id` cache via the targeted
    /// single-column update path (not the whole-row `save`), so the cache write
    /// appends NO snapshot and leaves `sync_state` untouched. A re-poll of the
    /// SAME value is idempotent (the no-op skip fires) and still writes nothing.
    #[tokio::test]
    async fn poll_once_caches_polled_status_and_is_idempotent() {
        use ports::TaskRepository;

        let projects = Arc::new(InMemoryProjectRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());
        let remote = Arc::new(InMemoryRemoteProjectProvider::new());

        let proj = project("PVT_kwHO_cache");
        projects.save(&proj).await.unwrap();

        let ws = WorkspaceId::new();
        let mut task = Task::new_draft(ws, None, "mirror".into()).unwrap();
        task.stage_for_sync().unwrap();
        task.promote_to_remote(RemoteRef::new("github", "1"))
            .unwrap();
        task.project_item_id = Some("PVTI_42".into());
        assert_eq!(task.project_status_option_id, None);
        let task_id = task.id;
        tasks.save(&task, SnapshotSource::Promote).await.unwrap();

        // Snapshot count is the whole-row-persist counter: each `save` appends
        // a row, but the targeted cache write must NOT. Capture the baseline.
        let snaps = tasks.snapshots_handle();
        let versions_after_promote = snaps.lock().unwrap().get(&task_id).map_or(0, Vec::len);

        remote.set_poll_items("PVT_kwHO_cache", vec![item("PVTI_42", Some("o_done"))]);

        let poller = poller(projects, tasks.clone(), remote.clone()).await;
        let report = poller.poll_once().await.unwrap();
        assert_eq!(report.items_matched, 1);

        // The cache was written via the targeted column update.
        let reloaded = tasks.get(task_id).await.unwrap();
        assert_eq!(
            reloaded.project_status_option_id.as_deref(),
            Some("o_done"),
            "the polled status_option_id is cached on the local task"
        );
        // The cache write is on a SEPARATE axis: sync_state stays Synced.
        assert_eq!(reloaded.sync, SyncState::Synced);
        // The targeted column write appends NO snapshot — the whole-row counter
        // is unchanged from the promote baseline.
        let versions_after_first = snaps.lock().unwrap().get(&task_id).map_or(0, Vec::len);
        assert_eq!(
            versions_after_first, versions_after_promote,
            "the targeted cache write must not append a snapshot"
        );

        // Re-poll the SAME value: the no-op skip fires, so nothing is persisted
        // (still no new snapshot row).
        let report2 = poller.poll_once().await.unwrap();
        assert_eq!(report2.items_matched, 1);
        let versions_after_second = snaps.lock().unwrap().get(&task_id).map_or(0, Vec::len);
        assert_eq!(
            versions_after_second, versions_after_first,
            "an unchanged re-poll must not re-persist the cache"
        );
    }

    /// No projects → an inert pass: the provider is never polled and the
    /// report is all-zero.
    #[tokio::test]
    async fn poll_once_with_no_projects_is_inert() {
        let projects = Arc::new(InMemoryProjectRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());
        let remote = Arc::new(InMemoryRemoteProjectProvider::new());

        let poller = poller(projects, tasks, remote.clone()).await;
        let report = poller.poll_once().await.unwrap();

        assert_eq!(report, PollReport::default());
        assert!(remote.calls().is_empty(), "no projects → never polled");
    }
}
