use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("connect: {0}")]
    Connect(#[from] sqlx::Error),
    #[error("migrate: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

/// Paired reader + writer pools for a single SQLite database.
///
/// SQLite serializes writers internally; opening eight unrestricted
/// connections only buys us `SQLITE_BUSY` retries. Instead we serialize
/// writes at the pool boundary (`writes: max_connections = 1`) and run
/// reads concurrently against WAL (`reads: max_connections = 4, read_only`).
///
/// `SqlitePool` is already `Arc`-backed, so `Db` is cheap to clone — every
/// adapter takes its own copy.
#[derive(Clone)]
pub struct Db {
    pub reads: SqlitePool,
    pub writes: SqlitePool,
}

/// Single-connection writer pool. WAL + a 10s busy timeout protect against
/// the rare cases where contention does spill over to the SQLite layer.
pub async fn open_write_pool(database_url: &str) -> Result<SqlitePool, PoolError> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(10));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    Ok(pool)
}

/// Four-connection read-only pool. Inherits WAL from the writer's setup —
/// must be opened *after* the writer has run migrations.
pub async fn open_read_pool(database_url: &str) -> Result<SqlitePool, PoolError> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .read_only(true)
        .foreign_keys(true)
        // Belt-and-suspenders for #110: disable prepared-statement caching on
        // the long-lived read pool. The primary fix pins every read query to an
        // explicit column list so `column_count()` is constant, but caching a
        // statement here is what let a stale `SELECT *` plan outlive a
        // cross-process `ALTER TABLE ... ADD COLUMN` — the live re-prepared
        // column count then overran sqlx's cached column metadata and panicked
        // a worker thread (index out of bounds), crashing the daemon tick. With
        // a zero-capacity cache every read re-prepares against the current
        // schema, so no future `SELECT *` regression can resurrect the panic.
        .statement_cache_capacity(0)
        .busy_timeout(Duration::from_secs(10));
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await?;
    Ok(pool)
}

/// Open both pools and run migrations. Order matters: the writer opens
/// first (creates the file + flips WAL on + applies schema) before the
/// reader connects.
pub async fn open_db(database_url: &str) -> Result<Db, PoolError> {
    let writes = open_write_pool(database_url).await?;
    crate::migrate(&writes).await?;
    crate::backfill_empty_repo_names(&writes).await?;
    // The friendly-IDs backfills depend on `repos.name` already being
    // populated (the prefix derives from it), so they run *after*
    // `backfill_empty_repo_names`.
    crate::backfill_empty_repo_prefixes(&writes).await?;
    crate::backfill_empty_task_hashes(&writes).await?;
    let reads = open_read_pool(database_url).await?;
    Ok(Db { reads, writes })
}

/// Convenience: open a `Db` from a filesystem path.
pub async fn open_from_path(path: &Path) -> Result<Db, PoolError> {
    let url = format!("sqlite://{}", path.display());
    open_db(&url).await
}
