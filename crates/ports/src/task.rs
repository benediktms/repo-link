//! Task-side repository contracts.

use async_trait::async_trait;
use domain_core::{RepoId, TaskId, Timestamp, WorkspaceId};
use domain_repo::RepoBinding;
use domain_sync::OutboxEntry;
use domain_task::{SnapshotSource, SyncState, Task, TaskSnapshot};
use domain_workspace::Workspace;

use crate::error::PortResult;
use crate::remote_task::RemoteComment;

// ---------- Workspace repository -----------------------------------------

#[async_trait]
pub trait WorkspaceRepository: Send + Sync {
    async fn save(&self, workspace: &Workspace) -> PortResult<()>;
    async fn get(&self, id: WorkspaceId) -> PortResult<Workspace>;
    async fn find_by_name(&self, name: &str) -> PortResult<Option<Workspace>>;
    async fn list(&self, include_archived: bool) -> PortResult<Vec<Workspace>>;
    async fn delete(&self, id: WorkspaceId) -> PortResult<()>;
}

// ---------- Repo binding repository --------------------------------------

#[async_trait]
pub trait RepoBindingRepository: Send + Sync {
    async fn save(&self, binding: &RepoBinding) -> PortResult<()>;
    async fn get(&self, id: RepoId) -> PortResult<RepoBinding>;
    async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> PortResult<Vec<RepoBinding>>;
    async fn find_by_canonical_url(
        &self,
        workspace_id: WorkspaceId,
        canonical_url: &str,
    ) -> PortResult<Option<RepoBinding>>;
    /// Look up a binding by its globally-unique `prefix`. Used by the
    /// repo locator path so callers can pass `--repo rpl` (or use
    /// `rpl-ak7` for tasks and reuse the prefix half here) instead of a
    /// UUID.
    async fn find_by_prefix(&self, prefix: &str) -> PortResult<Option<RepoBinding>>;
    /// Look up the live binding that holds a particular remote issue
    /// identity `(provider, remote_id)` within `workspace_id` via the
    /// `remote_mappings` table (D6 rekey — `remote_mappings` is
    /// keyed on `(filing_repo_id, provider, remote_id)` so the
    /// lookup is effectively a join). Used by the
    /// `rl repo doctor --repair` auto-target resolution chain
    /// (rpl-sv2) when the task's logical `repo_id` is itself stale
    /// and the doctor has to fall back to the remote-issue identity
    /// as the load-bearing signal.
    ///
    /// Scoped to `workspace_id` so a cross-workspace import
    /// (the same `(provider, remote_id)` happens to live in two
    /// workspaces) doesn't silently pick an arbitrary binding. The
    /// join against `repos` also filters out rows whose
    /// `filing_repo_id` references a deleted binding (silent-
    /// divergence protection — the doctor must never re-point a
    /// task to a *second* deleted binding).
    ///
    /// Returns `None` when no match OR when the match is
    /// ambiguous (multiple bindings in the same workspace hold
    /// the same `(provider, remote_id)` — `remote_mappings` has
    /// a `UNIQUE(provider, remote_id)` only across the cross-repo
    /// key, not on `(provider, remote_id)` alone). The doctor
    /// surfaces this as `unresolved` so the user can resolve
    /// it with `--target` rather than the service arbitrarily
    /// picking.
    async fn find_by_remote_mapping(
        &self,
        workspace_id: WorkspaceId,
        provider: &str,
        remote_id: &str,
    ) -> PortResult<Option<RepoId>>;
    async fn delete(&self, id: RepoId) -> PortResult<()>;
}

// ---------- Task repository -----------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct TaskFilter {
    pub workspace_id: Option<WorkspaceId>,
    pub repo_id: Option<RepoId>,
    /// Filter by the open/closed lifecycle bit (RFC 0004 D1). `Some(true)` =
    /// open only, `Some(false)` = closed only, `None` = both. Replaces the old
    /// `status: Option<TaskStatus>` filter; "blocked" is no longer a status
    /// (it's a relation), so blocked-filtering is done by the caller via
    /// [`domain_task::Task::is_blocked`] after loading.
    pub is_open: Option<bool>,
    /// Filter by sync state.
    pub sync_state: Option<SyncState>,
    /// Stale-scan predicate (RFC 0004 D3, poller): keep only tasks whose
    /// `synced_at` is NULL (never observed) or strictly older than this. When
    /// `Some`, `list` also orders by `synced_at ASC NULLS FIRST` so a `limit`
    /// takes the *stalest* first. `None` = no freshness filter, default order.
    pub synced_at_lt: Option<Timestamp>,
    /// Keep only project-backed tasks (`project_item_id IS NOT NULL`). The
    /// poller correlates these against polled board items.
    pub has_project_item_id: bool,
    /// JOIN `workspaces` and keep only tasks in a *pollable* workspace: `active`
    /// AND project-attached (`project_id IS NOT NULL`). The poller gate (RFC
    /// 0004 D3). Excluding projectless workspaces matters: a task with a stale
    /// `project_item_id` whose workspace has no project can never be reconciled
    /// or freshness-stamped, so without this gate it would sit at the front of
    /// the stale-scan forever, crowding out real candidates. `false` = no
    /// workspace filter.
    pub pollable_workspaces_only: bool,
    /// Cap the row count (RFC 0004 D3 poller `LIMIT`, default 200 at the call
    /// site). `None` = unbounded. Pairs with `synced_at_lt`'s ordering so the
    /// cap defers the freshest, not an arbitrary slice.
    pub limit: Option<usize>,
}

#[async_trait]
pub trait TaskRepository: Send + Sync {
    /// Persist `task` and append a new row to its snapshot history,
    /// tagged with `source`. The adapter assigns the next monotonic
    /// `version`. Both writes are committed in a single transaction.
    async fn save(&self, task: &Task, source: SnapshotSource) -> PortResult<()>;
    /// Persist several tasks (each with its own snapshot `source`) as a
    /// single atomic unit: either all of them land or none do. Callers use
    /// this when one logical change touches more than one task and a partial
    /// write would corrupt an invariant — e.g. the two sides of a reciprocal
    /// relation edge, where a half-written pair leaves the graph asymmetric.
    ///
    /// The default implementation loops over [`save`](Self::save) and is **not**
    /// atomic — it exists only so test doubles needn't reimplement it. Any
    /// adapter backed by real storage MUST override this with a single
    /// transaction wrapping every task's writes.
    async fn save_many(&self, tasks: &[(&Task, SnapshotSource)]) -> PortResult<()> {
        for (task, source) in tasks {
            self.save(task, *source).await?;
        }
        Ok(())
    }
    /// Persist the task row + its snapshot **and** the given outbox `entries`
    /// in a SINGLE atomic transaction — either all of them land or none do.
    /// This is the transactional-outbox guarantee (#54, CodeRabbit thread
    /// r3324166852): the task write and the enqueue of its outbound mutations
    /// can no longer tear apart, so a crash can never leave a saved mirror task
    /// with no durable outbox entry. Closes the draft-only / board-only gap the
    /// old save-then-enqueue path relied on the daemon's `DirtyLocal` reconcile
    /// to (partially) backstop.
    ///
    /// When `entries` is empty this MUST behave exactly like
    /// [`save`](Self::save) — the `LocalOnly` / no-op-edit path enqueues
    /// nothing and pays only for the task write.
    ///
    /// The default implementation is **not** atomic — it saves then enqueues
    /// through the two ports separately, exactly the tear-prone shape the
    /// dedicated method exists to replace. It is provided only so test doubles
    /// that don't exercise the combined path needn't reimplement it; any
    /// adapter backed by real storage MUST override it with one transaction.
    async fn save_with_outbox(
        &self,
        task: &Task,
        source: SnapshotSource,
        entries: &[OutboxEntry],
    ) -> PortResult<()> {
        self.save(task, source).await?;
        // NB: the default has no shared transaction handle, so this is a
        // best-effort fallback only. Real adapters override.
        let _ = entries;
        Ok(())
    }
    /// Persist several tasks (each with its snapshot `source`) **and** the given
    /// outbox `entries` in ONE atomic transaction — the [`save_many`] reciprocal
    /// guarantee and the [`save_with_outbox`] transactional-outbox guarantee
    /// combined. Relation sync needs exactly this: a `parent_of`/`blocked_by`
    /// edit writes the two reciprocal task rows together with the single
    /// outbound mutation it owes, and a torn write would leave the graph
    /// asymmetric OR the relation permanently unsynced (relations have no
    /// dirty-detection backstop, unlike title/body drift).
    ///
    /// When `entries` is empty this MUST behave exactly like [`save_many`].
    ///
    /// [`save_many`]: Self::save_many
    /// [`save_with_outbox`]: Self::save_with_outbox
    ///
    /// The default implementation is **not** atomic and, having no outbox
    /// handle, **drops `entries` entirely** — it persists the task rows and
    /// returns `Ok(())` without enqueuing anything (the same best-effort
    /// fallback as the [`save_with_outbox`] default). It exists ONLY so test
    /// doubles that never exercise the combined path needn't reimplement it; any
    /// adapter backed by real storage MUST override it with one transaction that
    /// writes the tasks AND the entries, or relation-sync mutations are silently
    /// lost (they have no dirty-detection backstop to re-enqueue them).
    async fn save_many_with_outbox(
        &self,
        tasks: &[(&Task, SnapshotSource)],
        entries: &[OutboxEntry],
    ) -> PortResult<()> {
        for (task, source) in tasks {
            self.save(task, *source).await?;
        }
        let _ = entries; // dropped: no outbox handle here — real adapters override
        Ok(())
    }
    async fn get(&self, id: TaskId) -> PortResult<Task>;
    async fn list(&self, filter: TaskFilter) -> PortResult<Vec<Task>>;
    /// Look up a task by its globally-unique `hash`. Used by the
    /// friendly-ID resolver so callers can pass a bare hash (`ak7`) or
    /// the prefix half of a composite (`rlk-ak7`) instead of a UUID.
    async fn find_by_hash(&self, hash: &str) -> PortResult<Option<Task>>;
    /// Look up the task mirroring a given remote issue within a repo
    /// (`filing_repo_id` + `provider` + `remote_id`). Scoped by the **filing**
    /// repo (RFC 0002 D6) — where the issue actually lives — because remote
    /// issue numbers are only unique per repo (GitHub `repoA#123` ≠
    /// `repoB#123`). Implementations COALESCE to the logical `repo_id` for rows
    /// whose filing repo is unresolved, so callers may pass the logical repo
    /// when filing == logical (e.g. `sync import`, which records both equal).
    /// Used by `sync import` to skip already-tracked issues.
    async fn find_by_remote(
        &self,
        filing_repo_id: RepoId,
        provider: &str,
        remote_id: &str,
    ) -> PortResult<Option<Task>>;
    /// Replace the task's *synced* comments with `comments` (always
    /// remote-backed — taking [`RemoteComment`] rather than `TaskComment`
    /// makes pending input unrepresentable), leaving any pending local-only
    /// comments untouched. Writes only the `task_comments` table — never a
    /// snapshot — so mirroring remote comments doesn't perturb sync state.
    async fn replace_comments(&self, task_id: TaskId, comments: &[RemoteComment])
    -> PortResult<()>;
    /// Append a single pending (local-only) comment, stored with the empty
    /// `remote_comment_id` sentinel. Writes only the `task_comments` table —
    /// never a snapshot — so adding a comment never perturbs sync state
    /// (pending comments are a separate outbound axis from title/body drift).
    async fn add_pending_comment(
        &self,
        task_id: TaskId,
        author: &str,
        body: &str,
        created_at: Timestamp,
    ) -> PortResult<()>;
    /// Promote a task's pending comments to synced after a successful remote
    /// push: deletes the rows in `drained_local_ids` and inserts `pushed` as
    /// synced rows. Writes only `task_comments`, never a snapshot.
    ///
    /// Identity-aware so the drain can't race-delete a pending comment that
    /// was added between the caller reading the task and this call: only the
    /// rows whose surrogate id was actually pushed are removed.
    async fn mark_comments_pushed(
        &self,
        task_id: TaskId,
        drained_local_ids: &[String],
        pushed: &[RemoteComment],
    ) -> PortResult<()>;
    /// Count pending (local-only) comments per task across a workspace, so
    /// `query unsynced` can surface comment-only outbound work without loading
    /// every task's comments (`list` deliberately skips them). Returns only
    /// tasks with at least one pending comment.
    async fn pending_comment_counts(
        &self,
        workspace_id: WorkspaceId,
    ) -> PortResult<std::collections::HashMap<TaskId, usize>>;
    /// Persist ONLY the `project_status_option_id` cache column for one task —
    /// a targeted single-column write that must NOT touch any other column,
    /// append a snapshot, bump the `version`, or change `sync_state`. The
    /// cached project-board status is a write-through hint orthogonal to the
    /// task aggregate (it's excluded from snapshots and the dirty diff), so it
    /// deliberately bypasses the whole-row `save`/aggregate path.
    ///
    /// Used by the poller's status reconcile (#56, closes #39, CodeRabbit
    /// thread r3325841752): the poller snapshots all tasks once per pass, so
    /// routing the cache write through `save` would clobber any title / body /
    /// status / sync_state edit a concurrent CLI made after that snapshot. A
    /// targeted column write can't tear those newer fields.
    ///
    /// Binding `option_id` as `None` clears the cache (writes SQL `NULL`). If
    /// no row matches `task_id` (task absent) this is a benign no-op — return
    /// `Ok` (the poller accounts for unmatched items separately).
    async fn cache_project_status(
        &self,
        task_id: TaskId,
        option_id: Option<String>,
    ) -> PortResult<()>;
    /// Backfill ONLY the `remote_node_id` column for one task — a targeted
    /// single-column write that must NOT touch any other column, append a
    /// snapshot, bump the `version`, or change `sync_state`. Used by `sync
    /// pull` to capture the GraphQL node id off a fetched REST snapshot for a
    /// pre-project-sync task whose `remote_id` was recorded before node ids
    /// were persisted (RFC 0001 §9 / §D1 — board eligibility).
    ///
    /// Routed off the whole-row `save` path for the same reason as
    /// [`cache_project_status`](Self::cache_project_status): pull's Noop branch
    /// does no aggregate write, and a whole-row save there could clobber a
    /// title / body / status edit a concurrent CLI made after the pull's read.
    /// `node_id` is invisible to dirty detection, so this never perturbs sync
    /// state.
    ///
    /// A node id only makes sense alongside a remote, so a remote-less
    /// (local-only / draft) task is a no-op — implementations must NOT strand a
    /// `node_id` on a row that has no remote. A zero-row match (task absent OR
    /// remote-less) is therefore benign.
    async fn cache_remote_node_id(&self, task_id: TaskId, node_id: String) -> PortResult<()>;
    /// Stamp ONLY the `synced_at` cache column for one task — the
    /// write-through "remote last observed" timestamp (RFC 0004 D3). A
    /// targeted single-column write in the same family as
    /// [`cache_project_status`](Self::cache_project_status) and
    /// [`cache_remote_node_id`](Self::cache_remote_node_id): it must NOT touch
    /// any other column, append a snapshot, bump the `version`, or change
    /// `sync_state` — observing the remote is on a separate axis from the
    /// mirrored content, so it can never flip dirty detection.
    ///
    /// Stamped by exactly three callers, each after a *confirmed* network
    /// response: the pull path, the drainer (push), and the poller. The
    /// `source` records which one; it is NOT persisted (there is no column for
    /// it) — it is carried so those write-through call sites (wired in later
    /// RFC 0004 phases) route through the `mark_synced` helper in
    /// `application-sync` with their own variant.
    ///
    /// A zero-row match (task absent) is a benign no-op — return `Ok`.
    async fn cache_synced_at(
        &self,
        task_id: TaskId,
        synced_at: Timestamp,
        source: SyncedSource,
    ) -> PortResult<()>;
    async fn delete(&self, id: TaskId) -> PortResult<()>;
}

/// Which of the three write-through callers stamped [`Task::synced_at`]
/// (RFC 0004 D3). Carried into [`TaskRepository::cache_synced_at`] via the
/// `mark_synced` helper in `application-sync` (the single funnel for the
/// stamp). Not persisted. The pull/push/poll call sites that pass these
/// variants are wired in later RFC 0004 phases.
///
/// [`Task::synced_at`]: domain_task::Task::synced_at
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncedSource {
    /// The `sync pull` path, after a successful fetch + apply.
    Pull,
    /// The outbox drainer, after a confirmed outbound mutation response.
    Push,
    /// The background project poller, after a successful per-task fetch.
    Polled,
    /// The on-demand `rl task show --refresh` observe (RFC 0004 D4): fetches
    /// the remote to stamp freshness only, without reconciling content.
    Refresh,
}

/// History queries over [`TaskSnapshot`] rows. Reads only — appends are
/// the side-effect of [`TaskRepository::save`] (so the snapshot table and
/// the task projection can't drift apart).
#[async_trait]
pub trait TaskSnapshotRepository: Send + Sync {
    /// All snapshots for a task, oldest version first.
    async fn list(&self, task_id: TaskId) -> PortResult<Vec<TaskSnapshot>>;

    /// Fetch a specific version. Returns `NotFound` if the version
    /// doesn't exist.
    async fn get(&self, task_id: TaskId, version: u64) -> PortResult<TaskSnapshot>;
}
