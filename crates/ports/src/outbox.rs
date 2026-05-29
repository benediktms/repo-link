//! Outbox repository port.

use async_trait::async_trait;
use domain_core::{OutboxEntryId, TaskId, Timestamp};
use domain_sync::OutboxEntry;

use crate::error::PortResult;

#[async_trait]
pub trait OutboxRepository: Send + Sync {
    async fn enqueue(&self, entry: &OutboxEntry) -> PortResult<()>;
    /// Atomic conditional enqueue for the startup dirty-reconcile (#54). Inserts
    /// `entry` only if its task has **no** non-terminal (`pending` / `inflight`)
    /// and **no** dead-lettered (`failed`) sibling — collapsing the reconcile's
    /// former `list_pending` + `list_failed` + `enqueue` round-trips into ONE
    /// transaction. Returns whether a row was inserted (`true`) or the guard
    /// short-circuited (`false`).
    ///
    /// Doing the check + insert atomically closes a race the separate calls
    /// left open: a concurrent CLI edit could enqueue a `pending` row AFTER the
    /// reconcile's checks passed but BEFORE its insert, producing a duplicate
    /// `UpdateRemote` for the same task. A `succeeded` sibling does NOT block —
    /// only non-terminal rows and the terminal dead-letter do (mirroring the
    /// reconcile's original `list_pending` / `list_failed` guards).
    async fn enqueue_if_absent(&self, entry: &OutboxEntry) -> PortResult<bool>;
    /// Per-task-FIFO claim with backoff eligibility (RFC 0001 §10.2). Selects
    /// the oldest `pending` entry that is eligible *now*
    /// (`next_attempt_at IS NULL OR next_attempt_at <= now`) whose task has
    /// **no earlier-enqueued non-terminal sibling** — i.e. no `inflight` entry
    /// and no older `pending` entry — and flips it to `inflight` in one
    /// transaction. Returns `None` when nothing is eligible.
    ///
    /// That "no earlier non-terminal sibling" guard is what makes the queue
    /// per-task serial while staying parallel across tasks: a stuck or
    /// in-progress head on task A never blocks task B, but A's own
    /// `start → edit → complete` sequence drains in enqueue order — even when
    /// the head is a backed-off `pending` (future `next_attempt_at`) and the
    /// tail is eligible now, the tail must wait behind the head.
    async fn claim_next_eligible(&self, now: Timestamp) -> PortResult<Option<OutboxEntry>>;
    /// Reset every `inflight` entry back to `pending` (clearing
    /// `next_attempt_at` so it is eligible immediately). Called once on daemon
    /// startup to recover entries orphaned by a crash / kill between the
    /// claim's inflight commit and the resolving write. Safe because the daemon
    /// is single-instance: at startup nothing is legitimately inflight, so any
    /// `inflight` row is a stranded claim. Returns the count reset.
    async fn requeue_orphaned_inflight(&self) -> PortResult<usize>;
    async fn mark_succeeded(&self, id: OutboxEntryId) -> PortResult<()>;
    /// Terminal dead-letter: set `status = 'failed'`, bump `attempts`, record
    /// `error`. The entry stays in the table for `rl sync outbox` visibility
    /// but is never re-claimed.
    async fn mark_failed(&self, id: OutboxEntryId, error: &str) -> PortResult<()>;
    /// Recoverable failure under the attempt cap: bump `attempts`, record
    /// `error`, set `next_attempt_at`, and flip `status` back to `pending` so
    /// the entry is re-claimed once the backoff window elapses.
    async fn record_retry(
        &self,
        id: OutboxEntryId,
        error: &str,
        next_attempt_at: Timestamp,
    ) -> PortResult<()>;
    async fn list_pending(&self, task_id: TaskId) -> PortResult<Vec<OutboxEntry>>;
    /// Delete every *pending* `AddItem` entry for one task. Used by the detach
    /// scrub (RFC 0001 §10.5): when a workspace detaches from a project, a
    /// still-pending board add for one of its tasks must not drain afterwards
    /// and re-anchor the task to the board it just left. Only `pending`
    /// `add_item` rows are removed — `inflight` / terminal rows are left as-is
    /// (an inflight add is already mid-flight; terminal rows are history).
    /// Returns the count deleted. Idempotent: a no-op when nothing matches, so
    /// a detach retry is safe.
    async fn delete_pending_add_items(&self, task_id: TaskId) -> PortResult<usize>;
    /// Every dead-lettered (`status = 'failed'`) entry for one task, newest
    /// update first. The startup dirty-reconcile uses this — alongside
    /// [`list_pending`](Self::list_pending) — as a re-enqueue blocker: a task
    /// that already dead-lettered is still `DirtyLocal`, but re-enqueuing it
    /// would silently bypass the attempt cap and retry forever across restarts.
    async fn list_failed(&self, task_id: TaskId) -> PortResult<Vec<OutboxEntry>>;
    /// Every dead-lettered (`status = 'failed'`) entry, newest update first.
    /// Backs `rl sync outbox` so a human can see what permanently failed.
    async fn list_dead_lettered(&self) -> PortResult<Vec<OutboxEntry>>;
}
