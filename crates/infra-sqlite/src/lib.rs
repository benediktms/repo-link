//! infra-sqlite — SQLite-backed implementations of the port traits.

mod event_sink;
mod mapping;
mod pool;
mod repo_binding_repo;
mod task_repo;
mod workspace_repo;

pub use event_sink::SqliteEventSink;
pub use pool::{Db, PoolError, open_db, open_from_path, open_read_pool, open_write_pool};
pub use repo_binding_repo::SqliteRepoBindingRepository;
pub use task_repo::SqliteTaskRepository;
pub use workspace_repo::SqliteWorkspaceRepository;

use sqlx::SqlitePool;

/// Run all embedded migrations. Called from `open_db` against the writer
/// pool already; exposed so callers using a hand-managed pool can re-run.
pub async fn migrate(pool: &SqlitePool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}
