//! infra-sqlite — SQLite-backed implementations of the port traits.

mod event_sink;
mod mapping;
mod migrate;
mod outbox_repo;
mod pool;
mod project_repo;
mod repo_binding_repo;
mod task_repo;
mod task_snapshot_repo;
mod workspace_repo;

pub use event_sink::SqliteEventSink;
pub use migrate::{
    backfill_empty_repo_names, backfill_empty_repo_prefixes, backfill_empty_task_hashes, migrate,
};
pub use outbox_repo::SqliteOutboxRepository;
pub use pool::{Db, PoolError, open_db, open_from_path, open_read_pool, open_write_pool};
pub use project_repo::SqliteProjectRepository;
pub use repo_binding_repo::SqliteRepoBindingRepository;
pub use task_repo::SqliteTaskRepository;
pub use task_snapshot_repo::SqliteTaskSnapshotRepository;
pub use workspace_repo::SqliteWorkspaceRepository;
