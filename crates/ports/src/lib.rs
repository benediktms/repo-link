//! ports — async trait contracts between application and infrastructure.

mod clock;
mod error;
mod event_sink;
mod filesystem;
mod outbox;
mod project;
mod remote_task;
mod task;

pub use clock::{Clock, SystemClock};
pub use error::{PortError, PortResult};
pub use event_sink::EventSink;
pub use filesystem::FilesystemProbe;
pub use outbox::OutboxRepository;
pub use project::{
    PollPage, ProjectRepository, RemoteProjectItem, RemoteProjectProvider, RemoteProjectSnapshot,
    RemoteProjectStatusOption,
};
pub use remote_task::{
    RemoteChildIssue, RemoteComment, RemoteStateReason, RemoteTaskCreate, RemoteTaskProvider,
    RemoteTaskSnapshot, RemoteTaskUpdate,
};
pub use task::{
    RepoBindingRepository, SyncedSource, TaskFilter, TaskRepository, TaskSnapshotRepository,
    WorkspaceRepository,
};
