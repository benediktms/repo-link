use std::sync::Mutex;

use async_trait::async_trait;
use dto_events::EventEnvelope;
use ports::{EventSink, PortResult};

// ---------- Event sink ----------------------------------------------------

#[derive(Default)]
pub struct CapturingEventSink {
    inner: Mutex<Vec<EventEnvelope>>,
}

impl CapturingEventSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<EventEnvelope> {
        self.inner.lock().unwrap().clone()
    }
}

#[async_trait]
impl EventSink for CapturingEventSink {
    async fn record(&self, envelope: EventEnvelope) -> PortResult<()> {
        self.inner.lock().unwrap().push(envelope);
        Ok(())
    }
}
