//! testing-fixtures — in-memory port impls + deterministic clock for tests.
//!
//! These are **pure-Rust** fakes backed by `Mutex<HashMap<Id, T>>` — no
//! SQLite, no sqlx, no tokio runtime spin-up beyond what `#[tokio::test]`
//! already does. They exist so that `application-*` unit tests can run
//! fast and side-effect-free.
//!
//! For tests that need the real adapter, exercise `infra-sqlite` directly
//! — those tests open an on-disk SQLite in a `tempfile::TempDir`. The
//! CLI end-to-end tests in `app-cli/tests/` also use a real on-disk
//! SQLite (necessary because the child process can't see the parent's
//! in-memory DB).

mod clock;
mod event_sink;
mod filesystem_probe;
mod outbox_repo;
mod project_repo;
mod repo_binding_repo;
mod task_repo;
mod task_snapshot_repo;
mod workspace_repo;

pub use clock::FixedClock;
pub use event_sink::CapturingEventSink;
pub use filesystem_probe::StubFilesystemProbe;
pub use outbox_repo::InMemoryOutboxRepository;
pub use project_repo::InMemoryProjectRepository;
pub use repo_binding_repo::InMemoryRepoBindingRepository;
pub use task_repo::InMemoryTaskRepository;
pub use task_snapshot_repo::InMemoryTaskSnapshotRepository;
pub use workspace_repo::InMemoryWorkspaceRepository;
