//! dto-events — domain event payloads for the audit log and future streams.

mod event;
mod payload;

pub use event::{DomainEvent, EventEnvelope};
pub use payload::{
    RepoAttached, RepoDetached, TaskArchived, TaskBlocked, TaskConflicted, TaskCreated,
    TaskDirtyLocal, TaskDirtyRemote, TaskPromoted, TaskStaged, TaskSynced, WorkspaceArchived,
    WorkspaceCreated, WorktreeMissing, WorktreePruned, WorktreeRegistered,
};
