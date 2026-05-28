//! Outbox repository port.

use async_trait::async_trait;
use domain_core::{OutboxEntryId, TaskId};
use domain_sync::OutboxEntry;

use crate::error::PortResult;

#[async_trait]
pub trait OutboxRepository: Send + Sync {
    async fn enqueue(&self, entry: &OutboxEntry) -> PortResult<()>;
    /// Atomically claim the oldest `pending` entry and mark it `inflight`.
    /// `None` when the queue is empty — the drainer can sleep until the
    /// next tick.
    async fn next_pending(&self) -> PortResult<Option<OutboxEntry>>;
    async fn mark_succeeded(&self, id: OutboxEntryId) -> PortResult<()>;
    async fn mark_failed(&self, id: OutboxEntryId, error: &str) -> PortResult<()>;
    async fn list_pending(&self, task_id: TaskId) -> PortResult<Vec<OutboxEntry>>;
}
