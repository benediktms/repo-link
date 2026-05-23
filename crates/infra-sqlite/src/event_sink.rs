use async_trait::async_trait;
use dto_events::EventEnvelope;
use ports::{EventSink, PortResult};

use crate::Db;
use crate::mapping::{json_to_string, map_sqlx_err};

pub struct SqliteEventSink {
    db: Db,
}

impl SqliteEventSink {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

#[async_trait]
impl EventSink for SqliteEventSink {
    async fn record(&self, envelope: EventEnvelope) -> PortResult<()> {
        let payload = json_to_string(&envelope.event)?;
        sqlx::query("INSERT INTO sync_events (at, workspace_id, payload_json) VALUES (?, ?, ?)")
            .bind(envelope.at)
            .bind(envelope.workspace_id.as_deref())
            .bind(payload)
            .execute(&self.db.writes)
            .await
            .map_err(map_sqlx_err)?;
        Ok(())
    }
}
