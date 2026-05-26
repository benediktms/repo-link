//! infra-sqlite — SQLite-backed implementations of the port traits.

mod event_sink;
mod mapping;
mod pool;
mod repo_binding_repo;
mod task_repo;
mod task_snapshot_repo;
mod workspace_repo;

pub use event_sink::SqliteEventSink;
pub use pool::{Db, PoolError, open_db, open_from_path, open_read_pool, open_write_pool};
pub use repo_binding_repo::SqliteRepoBindingRepository;
pub use task_repo::SqliteTaskRepository;
pub use task_snapshot_repo::SqliteTaskSnapshotRepository;
pub use workspace_repo::SqliteWorkspaceRepository;

use sqlx::{Row, SqlitePool};

/// Run all embedded migrations. Called from `open_db` against the writer
/// pool already; exposed so callers using a hand-managed pool can re-run.
pub async fn migrate(pool: &SqlitePool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

/// One-pass backfill: derive `name` for any repo whose `name` is empty,
/// using `domain_repo::derive_name(canonical_url)`. Idempotent — finds
/// nothing on a fully-backfilled DB.
///
/// The UPDATE re-asserts `name = ''` in the WHERE clause so a name set
/// concurrently between the initial SELECT and the per-row UPDATE
/// doesn't get stomped. The race window today is microscopic (this
/// runs at `open_db` time before the app starts writing), but in the
/// Phase D world where the daemon and CLI may share a DB it becomes
/// real — and the guard is free.
pub async fn backfill_empty_repo_names(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let rows = sqlx::query("SELECT id, canonical_url FROM repos WHERE name = ''")
        .fetch_all(pool)
        .await?;
    for row in rows {
        let id: String = row.try_get("id")?;
        let canonical_url: String = row.try_get("canonical_url")?;
        let name = domain_repo::derive_name(&canonical_url);
        sqlx::query("UPDATE repos SET name = ? WHERE id = ? AND name = ''")
            .bind(name)
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// Backfill `repos.prefix` for every legacy row created before the
/// friendly-IDs migration. Uses `domain_repo::derive_prefix(name)` as
/// the base value and appends a numeric suffix on UNIQUE collisions
/// (deterministic for the order the rows are visited; first row wins
/// the unsuffixed base).
///
/// Idempotent — the `WHERE prefix = ''` guard means a re-run finds
/// nothing once rows are populated. The per-row UPDATE also re-asserts
/// `prefix = ''` so two concurrent backfills can't both stomp a row.
pub async fn backfill_empty_repo_prefixes(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let rows = sqlx::query("SELECT id, name FROM repos WHERE prefix = ''")
        .fetch_all(pool)
        .await?;
    for row in rows {
        let id: String = row.try_get("id")?;
        let name: String = row.try_get("name")?;
        let base = domain_repo::derive_prefix(&name);
        let mut suffix: u32 = 0;
        loop {
            let candidate = if suffix == 0 {
                base.clone()
            } else {
                let s = suffix.to_string();
                let n = 8usize.saturating_sub(s.len());
                let trimmed: String = base.chars().take(n).collect();
                format!("{trimmed}{s}")
            };
            let res = sqlx::query("UPDATE repos SET prefix = ? WHERE id = ? AND prefix = ''")
                .bind(&candidate)
                .bind(&id)
                .execute(pool)
                .await;
            match res {
                Ok(r) if r.rows_affected() > 0 => break,
                // Race: another writer set the prefix between our SELECT
                // and UPDATE. Move on to the next row.
                Ok(_) => break,
                Err(e) if is_unique_violation(&e) => {
                    suffix += 1;
                    if suffix > 99 {
                        // Pathological: 100 repos sharing the same
                        // derived prefix. Surface as a backend error so
                        // the operator notices instead of silently
                        // skipping the row.
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

/// Backfill `tasks.hash` for every legacy row created before the
/// friendly-IDs migration. Mints a random lowercase base32 hash via
/// `domain_task::random_lowercase_base32`, retries on collision, and
/// grows the requested length after enough collisions at the same
/// length (matching the runtime `task create` mint behaviour).
pub async fn backfill_empty_task_hashes(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let rows = sqlx::query("SELECT id FROM tasks WHERE hash = ''")
        .fetch_all(pool)
        .await?;
    let mut length = 3usize;
    for row in rows {
        let id: String = row.try_get("id")?;
        let mut attempts_at_length: u32 = 0;
        loop {
            attempts_at_length += 1;
            let candidate = domain_task::random_lowercase_base32(length);
            let res = sqlx::query("UPDATE tasks SET hash = ? WHERE id = ? AND hash = ''")
                .bind(&candidate)
                .bind(&id)
                .execute(pool)
                .await;
            match res {
                Ok(r) if r.rows_affected() > 0 => break,
                Ok(_) => break, // raced — already filled
                Err(e) if is_unique_violation(&e) => {
                    if attempts_at_length >= 8 {
                        attempts_at_length = 0;
                        length += 1;
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}
