//! application-sync — orchestrates remote promotion / push / pull + the
//! outbound outbox drainer.
//!
//! Local SQLite is authoritative for draft state; once a task has been
//! pushed, GitHub becomes the source of truth. Sync transitions follow
//! [`SyncState`]; lifecycle ([`TaskStatus`]) is orthogonal and only
//! consulted to skip Archived tasks.
//!
//! Two outbound paths coexist within this stage:
//! - [`SyncService::push`] — the synchronous path kept for `rl task claim`'s
//!   interactive feedback and as a transitional fallback.
//! - [`OutboxDrainer`] — the asynchronous, retrying path the daemon drives.
//!   It is the sole outbound path the daemon uses (Stage 6 cutover).
//!
//! Both derive the remote `(closed, state_reason)` from the same
//! [`lifecycle_to_remote_state`] so a task closes/reopens identically no
//! matter which path flushes it.
//!
//! The inbound counterpart is [`ProjectPoller`] (Stage 7, #55): it pulls
//! project-board state back from GitHub on a cadence and correlates each
//! polled item with its local task. The daemon drives it as its own
//! concurrent task alongside the drainer.

mod drainer;
pub mod enqueue;
mod error;
mod poller;
mod service;
mod summary;

use domain_project::Project;
use domain_task::TaskStatus;
use ports::RemoteStateReason;

pub use drainer::{BackoffSchedule, OutboxDrainer};
pub use error::{Result, SyncError};
pub use poller::{PollReport, ProjectPoller};
pub use service::SyncService;

/// Resolve a task's lifecycle status to a project Status option id, applying
/// the RFC §3 absence-of-row rule: a `Blocked` task on a board with no
/// Blocked-like option (so no `blocked` mapping row was stored) falls back to
/// the `Open` option. Returns `None` only when even `Open` is unmapped (an
/// option-less board) — the caller then enqueues no `SetProjectStatus`.
///
/// Shared by the drainer's `AddItem` follow-up and the lifecycle / set-project
/// enqueue helpers so the fallback is applied identically at enqueue time.
pub fn option_id_for_status_with_fallback(project: &Project, status: TaskStatus) -> Option<String> {
    if let Some(opt) = project.option_id_for(status) {
        return Some(opt.to_string());
    }
    if status == TaskStatus::Blocked {
        // No row for Blocked ⇒ resolve to the Open option (app-level
        // fallback; never stored as a row — see RFC §3).
        return project.option_id_for(TaskStatus::Open).map(str::to_string);
    }
    None
}

/// Map a local lifecycle status onto the remote issue's open/closed bit plus
/// `state_reason`. `Done` closes as `Completed`; `Archived` closes as
/// `NotPlanned`; any open status reopens (we don't track whether the remote
/// was previously closed — sending `Reopened` unconditionally is harmless on
/// GitHub when the issue is already open and informative otherwise).
///
/// Shared by [`SyncService::push`] and the [`OutboxDrainer`] so the two
/// outbound paths can never diverge on how a lifecycle change maps to the
/// remote.
pub(crate) fn lifecycle_to_remote_state(status: TaskStatus) -> (bool, Option<RemoteStateReason>) {
    match status {
        TaskStatus::Done => (true, Some(RemoteStateReason::Completed)),
        TaskStatus::Archived => (true, Some(RemoteStateReason::NotPlanned)),
        TaskStatus::Open | TaskStatus::InProgress | TaskStatus::Blocked => {
            (false, Some(RemoteStateReason::Reopened))
        }
    }
}
