//! ports — async trait contracts between application and infrastructure.

mod clock;
mod error;
mod event_sink;
mod filesystem;
mod outbox;
mod project;
mod remote_task;
mod search;
mod task;

pub use clock::{Clock, SystemClock};
pub use error::{PortError, PortResult};
pub use event_sink::EventSink;
pub use filesystem::FilesystemProbe;
pub use outbox::OutboxRepository;
pub use project::{
    ItemStatusPage, OrgIssueTypeRepository, PollPage, ProjectRepository, RemoteIssueType,
    RemoteProjectField, RemoteProjectFieldOption, RemoteProjectItem, RemoteProjectProvider,
    RemoteProjectSnapshot,
};
pub use remote_task::{
    RemoteChildIssue, RemoteComment, RemoteStateReason, RemoteTaskCreate, RemoteTaskProvider,
    RemoteTaskSnapshot, RemoteTaskUpdate,
};
pub use search::{
    ChunkKind, ChunkTarget, CommentTextRow, EmbeddingProvider, GuardedVectorRow, IndexMetadata,
    IndexStats, LexicalRank, LiteralHit, MissingSemanticInput, ReconcileDiff, ReconcileFailure,
    ReconcileSession, SEARCH_CHUNK_FORMAT_VERSION, SEARCH_SCHEMA_VERSION, SchemaMismatch,
    SearchScope, SemanticRank, SidecarInfo, TaskIdentity, TaskSearchIndex,
    TaskSearchResultSnapshot, TaskSearchSourceRepository, TaskTextRow,
};
pub use task::{
    RepoBindingRepository, SyncedSource, TaskFilter, TaskRepository, TaskSnapshotRepository,
    WorkspaceRepository,
};
