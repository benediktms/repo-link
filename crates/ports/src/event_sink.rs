//! Event sink port.

use async_trait::async_trait;

use crate::error::PortResult;

// ---------- Event sink ---------------------------------------------------

#[async_trait]
pub trait EventSink: Send + Sync {
    async fn record(&self, envelope: dto_events::EventEnvelope) -> PortResult<()>;
}
