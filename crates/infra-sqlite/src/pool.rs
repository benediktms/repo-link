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
    let reads = open_read_pool(database_url).await?;
    Ok(Db { reads, writes })
}

/// Convenience: open a `Db` from a filesystem path.
pub async fn open_from_path(path: &Path) -> Result<Db, PoolError> {
    let url = format!("sqlite://{}", path.display());
    open_db(&url).await
}
