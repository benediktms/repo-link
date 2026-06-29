//! domain-core — IDs, timestamps, errors, base traits. No business logic.

mod aggregate;
mod error;
mod id;
mod time;

pub use aggregate::Aggregate;
pub use error::{DomainError, Result};
pub use id::{
    IdParseError, OutboxEntryId, ProjectId, ProjectIdParseError, RepoId, RepoInstanceId,
    RepoOriginId, TaskId, WorkspaceId,
};
pub use time::Timestamp;
