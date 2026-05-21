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
