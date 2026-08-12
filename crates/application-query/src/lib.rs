//! application-query — read-optimized views over the workspace.
//!
//! CQRS-light: each view returns a flat DTO shape ready for CLI rendering
//! or JSON output. No domain mutation lives here — `drift`'s live mode does
//! write, but only through [`ports::TaskRepository::cache_project_status`],
//! which refreshes a write-through cache column that is excluded from
//! snapshots and the dirty diff. No aggregate, version, or sync state moves.
//!
//! Status (lifecycle: Open / InProgress / Blocked / Done / Archived) and
//! sync state (LocalOnly / Staged / Synced / DirtyLocal / DirtyRemote /
//! Conflict) are surfaced as separate fields wherever both matter.

mod dto;
mod error;
mod service;

pub use dto::{
    AssignedTaskRow, BlockedTaskRow, ChildTaskRow, ChildrenRollup, ContributorRow,
    DriftCacheNotRefreshedNotice, DriftLiveUnavailableNotice, DriftPartiallyLiveNotice,
    DriftReport, DriftRow, LiveRead, QueryNoticeDto, ReadyNode, ReadyView, ReadyWorkspace,
    StaleWorktreeRow, UnsyncedTaskRow, WorkspaceOverview,
};
pub use error::{QueryError, Result};
pub use service::QueryService;
