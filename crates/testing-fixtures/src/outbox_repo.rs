use std::sync::Mutex;

use async_trait::async_trait;
use domain_core::{OutboxEntryId, TaskId, Timestamp};
use domain_sync::{OutboxEntry, OutboxStatus};
use ports::{OutboxRepository, PortResult};

// ---------- Outbox repository ---------------------------------------------

#[derive(Default)]
pub struct InMemoryOutboxRepository {
    /// Stored as an ordered `Vec` so `next_pending` can pop the oldest by
    /// `enqueued_at` without re-sorting on every call. Writes are
    /// append-only.
    inner: Mutex<Vec<OutboxEntry>>,
}

impl InMemoryOutboxRepository {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl OutboxRepository for InMemoryOutboxRepository {
    async fn enqueue(&self, entry: &OutboxEntry) -> PortResult<()> {
        self.inner.lock().unwrap().push(entry.clone());
        Ok(())
    }

    async fn next_pending(&self) -> PortResult<Option<OutboxEntry>> {
        let mut guard = self.inner.lock().unwrap();
        // Atomic claim: find the oldest pending entry by enqueued_at, flip
        // it to inflight in place, and return the post-flip view. Mirrors
        // the SQLite repo's transaction so test consumers see the same
        // shape.
        let oldest_idx = guard
            .iter()
            .enumerate()
            .filter(|(_, e)| e.status == OutboxStatus::Pending)
            .min_by_key(|(_, e)| e.enqueued_at)
            .map(|(i, _)| i);
        let Some(idx) = oldest_idx else {
            return Ok(None);
        };
        let entry = &mut guard[idx];
        entry.status = OutboxStatus::Inflight;
        entry.updated_at = Timestamp::now();
        Ok(Some(entry.clone()))
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
}
