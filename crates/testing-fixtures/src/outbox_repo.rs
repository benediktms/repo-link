use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use domain_core::{OutboxEntryId, TaskId, Timestamp};
use domain_sync::{OutboxEntry, OutboxStatus};
use ports::{OutboxRepository, PortResult};

// ---------- Outbox repository ---------------------------------------------

/// The shared append-only entry store. `Arc`-wrapped so
/// [`InMemoryTaskRepository`](crate::InMemoryTaskRepository) can hold a handle
/// to the SAME `Vec` and append task + outbox entries under one lock — giving
/// the in-memory `save_with_outbox` the same all-or-nothing atomicity the
/// SQLite adapter gets from a single transaction (#54).
pub(crate) type OutboxStore = Arc<Mutex<Vec<OutboxEntry>>>;

#[derive(Default)]
pub struct InMemoryOutboxRepository {
    /// Stored as an append-only `Vec`. The claim picks the oldest eligible
    /// entry by **insertion order** (the `Vec` index), mirroring the SQLite
    /// `rowid` FIFO/tie-break contract — NOT by `enqueued_at`, which is
    /// second-granular and so can't distinguish two same-task entries enqueued
    /// within the same second.
    inner: OutboxStore,
}

impl InMemoryOutboxRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// All entries (any status), in insertion order. Test-only inspection
    /// hook so drainer tests can assert on attempts / status / payload
    /// without going through the status-filtered query methods.
    pub fn all(&self) -> Vec<OutboxEntry> {
        self.inner.lock().unwrap().clone()
    }

    /// Hand out a clone of the shared entry-store handle so a paired
    /// [`InMemoryTaskRepository`](crate::InMemoryTaskRepository) can append into
    /// the SAME `Vec` for the transactional-outbox path (#54).
    pub(crate) fn store_handle(&self) -> OutboxStore {
        Arc::clone(&self.inner)
    }
}

#[async_trait]
impl OutboxRepository for InMemoryOutboxRepository {
    async fn enqueue(&self, entry: &OutboxEntry) -> PortResult<()> {
        self.inner.lock().unwrap().push(entry.clone());
        Ok(())
    }

    async fn enqueue_if_absent(&self, entry: &OutboxEntry) -> PortResult<bool> {
        // Check + insert under ONE lock so the dedupe guard and the push are
        // atomic against a concurrent enqueue (#54), mirroring the SQLite
        // adapter's single-transaction `INSERT ... WHERE NOT EXISTS`. The guard
        // fires on any non-terminal (`pending` / `inflight`) or dead-lettered
        // (`failed`) sibling for the same task.
        let mut guard = self.inner.lock().unwrap();
        let blocked = guard.iter().any(|e| {
            e.task_id == entry.task_id
                && matches!(
                    e.status,
                    OutboxStatus::Pending | OutboxStatus::Inflight | OutboxStatus::Failed
                )
        });
        if blocked {
            return Ok(false);
        }
        guard.push(entry.clone());
        Ok(true)
    }

    async fn claim_next_eligible(&self, now: Timestamp) -> PortResult<Option<OutboxEntry>> {
        let mut guard = self.inner.lock().unwrap();
        // FIFO/tie-break is keyed on the Vec **index** (insertion order), not
        // `enqueued_at`. This mirrors the SQLite `rowid` contract: two
        // same-task entries enqueued within the same (second-granular)
        // timestamp must still claim in insertion order, so tests need no
        // `sleep()` to stagger `enqueued_at`. A candidate is blocked if its
        // task has any earlier-*inserted* non-terminal sibling: an `inflight`
        // entry, or an earlier-indexed `pending` one. The latter is the
        // backed-off-head case — a head that failed recoverably is `pending`
        // with a future `next_attempt_at`, so it is not eligible, but its tail
        // must still wait behind it rather than overtaking it (per-task FIFO).
        let blocked = |idx: usize, e: &OutboxEntry| {
            guard.iter().enumerate().any(|(j, s)| {
                s.task_id == e.task_id
                    && (s.status == OutboxStatus::Inflight
                        || (s.status == OutboxStatus::Pending && j < idx))
            })
        };
        let oldest_idx = guard
            .iter()
            .enumerate()
            .find(|(idx, e)| {
                e.status == OutboxStatus::Pending
                    && e.next_attempt_at.map(|t| t <= now).unwrap_or(true)
                    && !blocked(*idx, e)
            })
            .map(|(i, _)| i);
        let Some(idx) = oldest_idx else {
            return Ok(None);
        };
        let entry = &mut guard[idx];
        entry.status = OutboxStatus::Inflight;
        entry.updated_at = Timestamp::now();
        Ok(Some(entry.clone()))
    }

    async fn requeue_orphaned_inflight(&self) -> PortResult<usize> {
        let mut guard = self.inner.lock().unwrap();
        let now = Timestamp::now();
        let mut reset = 0usize;
        for e in guard.iter_mut() {
            if e.status == OutboxStatus::Inflight {
                e.status = OutboxStatus::Pending;
                e.next_attempt_at = None;
                e.updated_at = now;
                reset += 1;
            }
        }
        Ok(reset)
    }

    async fn mark_succeeded(&self, id: OutboxEntryId) -> PortResult<()> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(entry) = guard.iter_mut().find(|e| e.id == id) {
            entry.status = OutboxStatus::Succeeded;
            entry.last_error = None;
            entry.updated_at = Timestamp::now();
        }
        Ok(())
    }

    async fn mark_failed(&self, id: OutboxEntryId, error: &str) -> PortResult<()> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(entry) = guard.iter_mut().find(|e| e.id == id) {
            entry.status = OutboxStatus::Failed;
            entry.last_error = Some(error.to_string());
            entry.attempts += 1;
            entry.updated_at = Timestamp::now();
        }
        Ok(())
    }

    async fn record_retry(
        &self,
        id: OutboxEntryId,
        error: &str,
        next_attempt_at: Timestamp,
    ) -> PortResult<()> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(entry) = guard.iter_mut().find(|e| e.id == id) {
            entry.status = OutboxStatus::Pending;
            entry.last_error = Some(error.to_string());
            entry.attempts += 1;
            entry.next_attempt_at = Some(next_attempt_at);
            entry.updated_at = Timestamp::now();
        }
        Ok(())
    }

    async fn list_pending(&self, task_id: TaskId) -> PortResult<Vec<OutboxEntry>> {
        let mut out: Vec<OutboxEntry> = self
            .inner
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.task_id == task_id && e.status == OutboxStatus::Pending)
            .cloned()
            .collect();
        out.sort_by_key(|e| e.enqueued_at);
        Ok(out)
    }

    async fn delete_pending_add_items(&self, task_id: TaskId) -> PortResult<usize> {
        let mut guard = self.inner.lock().unwrap();
        let before = guard.len();
        guard.retain(|e| {
            !(e.task_id == task_id
                && e.status == OutboxStatus::Pending
                && e.mutation.kind() == "add_item")
        });
        Ok(before - guard.len())
    }

    async fn list_failed(&self, task_id: TaskId) -> PortResult<Vec<OutboxEntry>> {
        let mut out: Vec<OutboxEntry> = self
            .inner
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.task_id == task_id && e.status == OutboxStatus::Failed)
            .cloned()
            .collect();
        out.sort_by_key(|e| std::cmp::Reverse(e.updated_at));
        Ok(out)
    }

    async fn list_dead_lettered(&self) -> PortResult<Vec<OutboxEntry>> {
        let mut out: Vec<OutboxEntry> = self
            .inner
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.status == OutboxStatus::Failed)
            .cloned()
            .collect();
        out.sort_by_key(|e| std::cmp::Reverse(e.updated_at));
        Ok(out)
    }
}
