//! Task-side repository contracts.

use async_trait::async_trait;
use domain_core::{RepoId, TaskId, Timestamp, WorkspaceId};
use domain_repo::RepoBinding;
use domain_sync::OutboxEntry;
use domain_task::{SnapshotSource, SyncState, Task, TaskSnapshot, TaskStatus};
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
    async fn delete(&self, id: RepoId) -> PortResult<()>;
}

// ---------- Task repository -----------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct TaskFilter {
    pub workspace_id: Option<WorkspaceId>,
    pub repo_id: Option<RepoId>,
    /// Filter by lifecycle status. When `None`, callers usually want
    /// non-archived rows only — see `include_archived`.
    pub status: Option<TaskStatus>,
    /// Filter by sync state.
    pub sync_state: Option<SyncState>,
    /// When `status` is `None`, include `Archived` rows. Ignored if
    /// `status` is set explicitly.
    pub include_archived: bool,
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
    async fn get(&self, id: TaskId) -> PortResult<Task>;
    async fn list(&self, filter: TaskFilter) -> PortResult<Vec<Task>>;
    /// Look up a task by its globally-unique `hash`. Used by the
    /// friendly-ID resolver so callers can pass a bare hash (`ak7`) or
    /// the prefix half of a composite (`rlk-ak7`) instead of a UUID.
    async fn find_by_hash(&self, hash: &str) -> PortResult<Option<Task>>;
    /// Look up the task mirroring a given remote issue within a repo
    /// (`repo_id` + `provider` + `remote_id`). Scoped by repo because remote
    /// issue numbers are only unique per repo (GitHub `repoA#123` ≠
    /// `repoB#123`). Used by `sync import` to skip already-tracked issues.
    async fn find_by_remote(
        &self,
        repo_id: RepoId,
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
    async fn delete(&self, id: TaskId) -> PortResult<()>;
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
