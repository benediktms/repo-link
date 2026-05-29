use async_trait::async_trait;
use chrono::{DateTime, Utc};
use domain_core::{OutboxEntryId, TaskId, Timestamp};
use domain_sync::{OutboxEntry, OutboxMutation, OutboxStatus};
use ports::{OutboxRepository, PortError, PortResult};
use sqlx::{Row, Sqlite};

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

const SELECT_COLS: &str = "id, task_id, mutation_kind, payload_json, status, attempts, last_error, next_attempt_at, enqueued_at, updated_at";

/// Insert one [`OutboxEntry`] row inside an existing transaction. Shared by
/// [`SqliteOutboxRepository::enqueue`] (its own one-statement tx via the writer
/// pool) and [`SqliteTaskRepository::save_with_outbox`] (folded into the task
/// write's transaction for the transactional-outbox guarantee, #54). Keeping
/// the INSERT in one place means the task write and the enqueue can't drift
/// apart on column order / payload encoding.
pub(crate) async fn insert_outbox_in_tx(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    entry: &OutboxEntry,
) -> PortResult<()> {
    sqlx::query(
        r#"
        INSERT INTO outbox_entries
            (id, task_id, mutation_kind, payload_json, status, attempts, last_error, next_attempt_at, enqueued_at, updated_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(entry.id.to_string())
    .bind(entry.task_id.to_string())
    .bind(entry.mutation.kind())
    .bind(json_to_string(&entry.mutation)?)
    .bind(enum_to_str(&entry.status)?)
    .bind(i64::from(entry.attempts))
    .bind(entry.last_error.as_deref())
    .bind(entry.next_attempt_at.map(Timestamp::into_inner))
    .bind(entry.enqueued_at.into_inner())
    .bind(entry.updated_at.into_inner())
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx_err)?;
    Ok(())
}

#[async_trait]
impl OutboxRepository for SqliteOutboxRepository {
    async fn enqueue(&self, entry: &OutboxEntry) -> PortResult<()> {
        // Single-statement write through the writer pool. Reuses the same
        // in-tx INSERT as `save_with_outbox` so the two enqueue surfaces stay
        // byte-identical.
        let mut tx = self
            .db
            .writes
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_sqlx_err)?;
        insert_outbox_in_tx(&mut tx, entry).await?;
        tx.commit().await.map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn enqueue_if_absent(&self, entry: &OutboxEntry) -> PortResult<bool> {
        // One `BEGIN IMMEDIATE` transaction (writer lock) so the dedupe guard
        // and the insert are atomic against a concurrent CLI edit (#54): the
        // `WHERE NOT EXISTS` is evaluated under the same lock that performs the
        // insert, so no `pending` row can slip in between the check and the
        // write. The guard fires on any non-terminal (`pending` / `inflight`)
        // or dead-lettered (`failed`) sibling for the same task — the same set
        // the reconcile's former `list_pending` + `list_failed` guards covered.
        let mut tx = self
            .db
            .writes
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_sqlx_err)?;
        let result = sqlx::query(
            r#"
            INSERT INTO outbox_entries
                (id, task_id, mutation_kind, payload_json, status, attempts, last_error, next_attempt_at, enqueued_at, updated_at)
            SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
             WHERE NOT EXISTS (
                 SELECT 1 FROM outbox_entries
                  WHERE task_id = ?
                    AND status IN ('pending', 'inflight', 'failed')
             )
            "#,
        )
        .bind(entry.id.to_string())
        .bind(entry.task_id.to_string())
        .bind(entry.mutation.kind())
        .bind(json_to_string(&entry.mutation)?)
        .bind(enum_to_str(&entry.status)?)
        .bind(i64::from(entry.attempts))
        .bind(entry.last_error.as_deref())
        .bind(entry.next_attempt_at.map(Timestamp::into_inner))
        .bind(entry.enqueued_at.into_inner())
        .bind(entry.updated_at.into_inner())
        .bind(entry.task_id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_err)?;
        tx.commit().await.map_err(map_sqlx_err)?;
        Ok(result.rows_affected() > 0)
    }

    async fn claim_next_eligible(&self, now: Timestamp) -> PortResult<Option<OutboxEntry>> {
        // One transaction (BEGIN IMMEDIATE → writer lock) so the SELECT and the
        // flip-to-inflight are atomic against a second drainer. Eligibility
        // honours the backoff window.
        //
        // Per-task FIFO: the candidate must have NO earlier-enqueued
        // non-terminal sibling — neither an `inflight` entry NOR an older
        // `pending` one. The inflight half is the obvious "A's head is in
        // progress ⇒ A's tail waits". The older-pending half is the subtle but
        // load-bearing case: when a head fails recoverably it goes back to
        // `pending` with a *future* `next_attempt_at`, so it is not eligible —
        // but its tail (eligible now) must NOT overtake it, or the task's
        // mutations reorder. Excluding any candidate that has an older pending
        // sibling keeps the head ahead of its tail. B is still claimable
        // because the guard is keyed on `task_id`.
        let mut tx = self
            .db
            .writes
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_sqlx_err)?;

        let now_inner = now.into_inner();
        // `enqueued_at` is second-granular, so two same-task entries enqueued
        // in the same second carry equal timestamps and wouldn't block each
        // other under an `enqueued_at`-only sibling predicate — breaking
        // per-task FIFO. Tie-break on SQLite's implicit `rowid` (monotonic
        // insertion order, no migration) so a later-inserted sibling always
        // sorts after an equal-timestamped earlier one. The secondary
        // `ORDER BY o.rowid` makes the outer pick deterministic for the same
        // reason.
        let row = sqlx::query(&format!(
            r#"
            SELECT {SELECT_COLS}, o.rowid AS _rowid
              FROM outbox_entries o
             WHERE o.status = 'pending'
               AND (o.next_attempt_at IS NULL OR o.next_attempt_at <= ?)
               AND NOT EXISTS (
                   SELECT 1 FROM outbox_entries s
                    WHERE s.task_id = o.task_id
                      AND (
                          s.status = 'inflight'
                          OR (s.status = 'pending'
                              AND (s.enqueued_at < o.enqueued_at
                                   OR (s.enqueued_at = o.enqueued_at
                                       AND s.rowid < o.rowid)))
                      )
               )
             ORDER BY o.enqueued_at ASC, o.rowid ASC
             LIMIT 1
            "#
        ))
        .bind(now_inner)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx_err)?;

        let Some(row) = row else {
            tx.commit().await.map_err(map_sqlx_err)?;
            return Ok(None);
        };

        let id_str: String = row.try_get("id").map_err(map_sqlx_err)?;
        let flip_at = Timestamp::now();
        sqlx::query("UPDATE outbox_entries SET status = 'inflight', updated_at = ? WHERE id = ?")
            .bind(flip_at.into_inner())
            .bind(&id_str)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;
        tx.commit().await.map_err(map_sqlx_err)?;

        let mut entry = row_to_entry(&row)?;
        entry.status = OutboxStatus::Inflight;
        entry.updated_at = flip_at;
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

    async fn record_retry(
        &self,
        id: OutboxEntryId,
        error: &str,
        next_attempt_at: Timestamp,
    ) -> PortResult<()> {
        let now = Timestamp::now();
        sqlx::query(
            r#"
            UPDATE outbox_entries
               SET status = 'pending',
                   last_error = ?,
                   attempts = attempts + 1,
                   next_attempt_at = ?,
                   updated_at = ?
             WHERE id = ?
            "#,
        )
        .bind(error)
        .bind(next_attempt_at.into_inner())
        .bind(now.into_inner())
        .bind(id.to_string())
        .execute(&self.db.writes)
        .await
        .map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn requeue_orphaned_inflight(&self) -> PortResult<usize> {
        let now = Timestamp::now();
        let result = sqlx::query(
            r#"
            UPDATE outbox_entries
               SET status = 'pending',
                   next_attempt_at = NULL,
                   updated_at = ?
             WHERE status = 'inflight'
            "#,
        )
        .bind(now.into_inner())
        .execute(&self.db.writes)
        .await
        .map_err(map_sqlx_err)?;
        Ok(usize::try_from(result.rows_affected()).unwrap_or(usize::MAX))
    }

    async fn list_pending(&self, task_id: TaskId) -> PortResult<Vec<OutboxEntry>> {
        let rows = sqlx::query(&format!(
            r#"
            SELECT {SELECT_COLS}
              FROM outbox_entries
             WHERE task_id = ? AND status = 'pending'
             ORDER BY enqueued_at ASC
            "#
        ))
        .bind(task_id.to_string())
        .fetch_all(&self.db.reads)
        .await
        .map_err(map_sqlx_err)?;

        rows.iter().map(row_to_entry).collect()
    }

    async fn delete_pending_add_items(&self, task_id: TaskId) -> PortResult<usize> {
        let result = sqlx::query(
            r#"
            DELETE FROM outbox_entries
             WHERE task_id = ? AND status = 'pending' AND mutation_kind = 'add_item'
            "#,
        )
        .bind(task_id.to_string())
        .execute(&self.db.writes)
        .await
        .map_err(map_sqlx_err)?;
        Ok(usize::try_from(result.rows_affected()).unwrap_or(usize::MAX))
    }

    async fn list_failed(&self, task_id: TaskId) -> PortResult<Vec<OutboxEntry>> {
        let rows = sqlx::query(&format!(
            r#"
            SELECT {SELECT_COLS}
              FROM outbox_entries
             WHERE task_id = ? AND status = 'failed'
             ORDER BY updated_at DESC
            "#
        ))
        .bind(task_id.to_string())
        .fetch_all(&self.db.reads)
        .await
        .map_err(map_sqlx_err)?;

        rows.iter().map(row_to_entry).collect()
    }

    async fn list_dead_lettered(&self) -> PortResult<Vec<OutboxEntry>> {
        let rows = sqlx::query(&format!(
            r#"
            SELECT {SELECT_COLS}
              FROM outbox_entries
             WHERE status = 'failed'
             ORDER BY updated_at DESC
            "#
        ))
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
    let next_attempt_at: Option<DateTime<Utc>> =
        row.try_get("next_attempt_at").map_err(map_sqlx_err)?;
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
        next_attempt_at: next_attempt_at.map(Timestamp::from_utc),
        enqueued_at: Timestamp::from_utc(enqueued_at),
        updated_at: Timestamp::from_utc(updated_at),
    })
}
