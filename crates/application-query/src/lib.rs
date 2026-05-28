//! application-query — read-optimized views over the workspace.
//!
//! CQRS-light: each view returns a flat DTO shape ready for CLI rendering
//! or JSON output. No domain mutation lives here.
//!
//! Status (lifecycle: Open / InProgress / Blocked / Done / Archived) and
//! sync state (LocalOnly / Staged / Synced / DirtyLocal / DirtyRemote /
//! Conflict) are surfaced as separate fields wherever both matter.

mod dto;
mod error;
mod service;

pub use dto::{
    AssignedTaskRow, BlockedTaskRow, ContributorRow, DriftRow, ReadyTaskRow, StaleWorktreeRow,
    UnsyncedTaskRow, WorkspaceOverview,
};
pub use error::{QueryError, Result};
pub use service::QueryService;
