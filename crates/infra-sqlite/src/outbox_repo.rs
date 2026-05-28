use async_trait::async_trait;
use chrono::{DateTime, Utc};
use domain_core::{OutboxEntryId, TaskId, Timestamp};
use domain_sync::{OutboxEntry, OutboxMutation, OutboxStatus};
use ports::{OutboxRepository, PortError, PortResult};
use sqlx::Row;

use crate::Db;
use crate::mapping::{
    enum_from_str, enum_to_str, json_from_string, json_to_string, map_sqlx_err, parse_uuid,
};

pub struct SqliteOutboxRepository {
    db: Db,
}

impl SqliteOutboxRepository {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

#[async_trait]
impl OutboxRepository for SqliteOutboxRepository {
    async fn enqueue(&self, entry: &OutboxEntry) -> PortResult<()> {
        sqlx::query(
            r#"
            INSERT INTO outbox_entries
                (id, task_id, mutation_kind, payload_json, status, attempts, last_error, enqueued_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(entry.id.to_string())
        .bind(entry.task_id.to_string())
        .bind(entry.mutation.kind())
        .bind(json_to_string(&entry.mutation)?)
        .bind(enum_to_str(&entry.status)?)
        .bind(i64::from(entry.attempts))
        .bind(entry.last_error.as_deref())
        .bind(entry.enqueued_at.into_inner())
        .bind(entry.updated_at.into_inner())
        .execute(&self.db.writes)
        .await
        .map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn next_pending(&self) -> PortResult<Option<OutboxEntry>> {
        // Atomically claim the oldest pending row and mark it inflight in
        // one transaction so two concurrent drainers can't both pull the
        // same entry. BEGIN IMMEDIATE grabs the writer lock up front to
        // avoid the deferred-upgrade SQLITE_BUSY trap (same pattern as
        // `SqliteRepoBindingRepository::save`).
        let mut tx = self
            .db
            .writes
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_sqlx_err)?;

        let row = sqlx::query(
            r#"
            SELECT id, task_id, mutation_kind, payload_json, status, attempts, last_error, enqueued_at, updated_at
              FROM outbox_entries
             WHERE status = 'pending'
             ORDER BY enqueued_at ASC
             LIMIT 1
            "#,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx_err)?;

        let Some(row) = row else {
            // Empty queue. Commit the empty transaction so the caller can
            // start another one cleanly.
            tx.commit().await.map_err(map_sqlx_err)?;
            return Ok(None);
        };

        let id_str: String = row.try_get("id").map_err(map_sqlx_err)?;
        let now = Timestamp::now();
        sqlx::query("UPDATE outbox_entries SET status = 'inflight', updated_at = ? WHERE id = ?")
            .bind(now.into_inner())
            .bind(&id_str)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;
        tx.commit().await.map_err(map_sqlx_err)?;

        // Decode the row we just claimed. The status is `inflight` now
        // even though the SELECT saw it as `pending` — return the
        // post-update view so the caller sees the world it now operates
        // against.
        let mut entry = row_to_entry(&row)?;
        entry.status = OutboxStatus::Inflight;
        entry.updated_at = now;
        Ok(Some(entry))
    }

    async fn mark_succeeded(&self, id: OutboxEntryId) -> PortResult<()> {
        let now = Timestamp::now();
        sqlx::query(
            "UPDATE outbox_entries SET status = 'succeeded', last_error = NULL, updated_at = ? WHERE id = ?",
        )
        .bind(now.into_inner())
        .bind(id.to_string())
        .execute(&self.db.writes)
        .await
        .map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn mark_failed(&self, id: OutboxEntryId, error: &str) -> PortResult<()> {
        let now = Timestamp::now();
        sqlx::query(
            r#"
            UPDATE outbox_entries
               SET status = 'failed',
                   last_error = ?,
                   attempts = attempts + 1,
                   updated_at = ?
             WHERE id = ?
            "#,
        )
        .bind(error)
        .bind(now.into_inner())
        .bind(id.to_string())
        .execute(&self.db.writes)
        .await
        .map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn list_pending(&self, task_id: TaskId) -> PortResult<Vec<OutboxEntry>> {
        let rows = sqlx::query(
            r#"
            SELECT id, task_id, mutation_kind, payload_json, status, attempts, last_error, enqueued_at, updated_at
              FROM outbox_entries
             WHERE task_id = ? AND status = 'pending'
             ORDER BY enqueued_at ASC
            "#,
        )
        .bind(task_id.to_string())
        .fetch_all(&self.db.reads)
        .await
        .map_err(map_sqlx_err)?;

        rows.iter().map(row_to_entry).collect()
    }
}

fn row_to_entry(row: &sqlx::sqlite::SqliteRow) -> PortResult<OutboxEntry> {
    let id_str: String = row.try_get("id").map_err(map_sqlx_err)?;
    let task_id_str: String = row.try_get("task_id").map_err(map_sqlx_err)?;
    let _kind: String = row.try_get("mutation_kind").map_err(map_sqlx_err)?;
    let payload_json: String = row.try_get("payload_json").map_err(map_sqlx_err)?;
    let status: String = row.try_get("status").map_err(map_sqlx_err)?;
    let attempts_raw: i64 = row.try_get("attempts").map_err(map_sqlx_err)?;
    let last_error: Option<String> = row.try_get("last_error").map_err(map_sqlx_err)?;
    let enqueued_at: DateTime<Utc> = row.try_get("enqueued_at").map_err(map_sqlx_err)?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at").map_err(map_sqlx_err)?;

    // `mutation_kind` is redundant with the embedded `kind` tag in
    // `payload_json` — we deserialize from the payload (which keeps the
    // discriminator) and ignore the column. The column exists so a future
    // index / WHERE clause can filter without parsing JSON.
    let mutation: OutboxMutation = json_from_string("outbox_entries.payload_json", &payload_json)?;
    let attempts = u32::try_from(attempts_raw)
        .map_err(|e| PortError::Backend(format!("outbox attempts overflow: {e}")))?;

    Ok(OutboxEntry {
        id: parse_uuid::<OutboxEntryId>("outbox_entries.id", &id_str)?,
        task_id: parse_uuid::<TaskId>("outbox_entries.task_id", &task_id_str)?,
        mutation,
        status: enum_from_str::<OutboxStatus>("outbox_entries.status", &status)?,
        attempts,
        last_error,
        enqueued_at: Timestamp::from_utc(enqueued_at),
        updated_at: Timestamp::from_utc(updated_at),
    })
}
