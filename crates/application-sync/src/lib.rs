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
use domain_task::{MirrorPatch, Task, TaskStatus};
use ports::{RemoteStateReason, RemoteTaskUpdate};

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
///
/// Delegates to the canonical [`Project::resolved_option_id_for`] so the
/// outbox enqueue/drain paths and Stage 8 drift detection (which calls the
/// domain resolver directly) can never diverge on the fallback definition —
/// there is exactly ONE Blocked→Open rule, and it lives in `domain-project`.
/// Keeps the `Option<String>` shape its callers expect by mapping the `&str`.
pub fn option_id_for_status_with_fallback(project: &Project, status: TaskStatus) -> Option<String> {
    project.resolved_option_id_for(status).map(str::to_string)
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

/// Build a [`RemoteTaskUpdate`] from the live `task`'s field-level diff
/// against its `synced_baseline` (RFC 0003 D4). Returns `None` when the
/// patch is empty (no mirrored field changed) so the caller can skip the
/// remote PATCH entirely — a no-op push, a coalesced empty head, a
/// comment-only push, etc. all collapse to "no remote call" instead of a
/// wasted PATCH.
///
/// Shared by [`SyncService::push`] and [`OutboxDrainer::apply`]'s
/// `UpdateRemote` arm so the two outbound paths can never diverge on which
/// fields ride a PATCH or when the call is skipped.
///
/// Status mapping delegates to [`lifecycle_to_remote_state`]: when the
/// patch carries a `status` change, the helper projects it onto
/// `(closed, state_reason)` the same way the old hand-rolled builders
/// did, so the wire shape is unchanged for status-only edits.
///
/// Assignees: a `Some(patch.assignees)` is forwarded as
/// `Some(&patch.assignees)`; the existing `Some(&[])` → clear vs
/// `None` → omit three-state semantics in the adapter (rpl-ffv) handle
/// the rest.
///
/// D7 (draft-backed mirrors): the helper is never called for a
/// draft-backed mirror. The structural reason is upstream — the
/// planner (`enqueue::plan_mutations` at
/// `crates/application-sync/src/enqueue.rs:198-208`) only emits an
/// `OutboxMutation::UpdateRemote` for `is_issue_backed` tasks; a
/// draft-backed mirror routes to `UpdateDraftIssue` instead, which
/// goes through the `update_draft_issue` port, not the
/// `update_remote` one. And push's `NoRemote` guard rejects
/// `task.remote.is_none()` upfront. So a draft-backed task is
/// structurally unreachable from this helper. The carve-out is
/// therefore free.
pub(crate) fn build_update_from_patch<'a>(
    _task: &Task,
    patch: &'a MirrorPatch,
    canonical_repo: &'a str,
    remote_id: &'a str,
) -> Option<RemoteTaskUpdate<'a>> {
    if patch.is_empty() {
        return None;
    }
    // Status projection: when the patch carries a status change, project
    // it through `lifecycle_to_remote_state` in one place. The match
    // makes the "closed + state_reason come together or not at all"
    // coupling locally obvious; a `match` survives any future
    // `lifecycle_to_remote_state` arm that returns `(_, None)`.
    let (closed, state_reason) = match patch.status {
        Some(s) => {
            let (c, r) = lifecycle_to_remote_state(s);
            (Some(c), r)
        }
        None => (None, None),
    };
    Some(RemoteTaskUpdate {
        canonical_repo,
        remote_id,
        title: patch.title.as_deref(),
        body: patch.body.as_deref(),
        closed,
        state_reason,
        assignees: patch.assignees.as_deref(),
    })
}

#[cfg(test)]
mod build_update_from_patch_tests {
    use super::*;
    use domain_task::TaskStatus;

    // A trivial Task — the helper ignores everything except the patch, so the
    // task value itself is never inspected.
    fn any_task() -> Task {
        // The trait bounds on Task::new_draft (title non-empty) mean we need a
        // minimal valid task. We use the workhorse since the helper is
        // field-agnostic; only the patch contents drive the projection.
        use domain_core::{Timestamp, WorkspaceId};
        let mut t = Task::new_draft(WorkspaceId::new(), None, "placeholder".into()).unwrap();
        t.id = domain_core::TaskId::new();
        t.created_at = Timestamp::now();
        t.updated_at = Timestamp::now();
        t
    }

    #[test]
    fn none_when_patch_empty() {
        // Default-constructed MirrorPatch carries no Some-field — the helper
        // must short-circuit before constructing a RemoteTaskUpdate at all.
        let patch = MirrorPatch::default();
        assert!(patch.is_empty());
        assert!(build_update_from_patch(&any_task(), &patch, "github.com/o/r", "1").is_none());
    }

    #[test]
    fn title_only_projects_only_title() {
        // A title-only diff: title is the only Some(_), every other port
        // field is None. Closed/state_reason stay None because status isn't
        // in the patch (so the helper must NOT project lifecycle_to_remote_state
        // on an absent status).
        let patch = MirrorPatch {
            title: Some("new title".into()),
            ..Default::default()
        };
        let u = build_update_from_patch(&any_task(), &patch, "github.com/o/r", "7")
            .expect("patch has title ⇒ Some");
        assert_eq!(u.canonical_repo, "github.com/o/r");
        assert_eq!(u.remote_id, "7");
        assert_eq!(u.title, Some("new title"));
        assert_eq!(u.body, None);
        assert_eq!(u.closed, None);
        assert_eq!(u.state_reason, None);
        assert_eq!(u.assignees, None);
    }

    #[test]
    fn status_done_projects_to_closed_completed() {
        // A status-only diff: patch.status = Done ⇒ helper runs
        // lifecycle_to_remote_state, gets (true, Some(Completed)) ⇒ sets
        // closed=Some(true) and state_reason=Some(Completed). Title/body/
        // assignees stay None (status is the only changed field).
        let patch = MirrorPatch {
            status: Some(TaskStatus::Done),
            ..Default::default()
        };
        let u = build_update_from_patch(&any_task(), &patch, "github.com/o/r", "1")
            .expect("patch has status ⇒ Some");
        assert_eq!(u.title, None);
        assert_eq!(u.body, None);
        assert_eq!(u.closed, Some(true));
        assert_eq!(u.state_reason, Some(RemoteStateReason::Completed));
        assert_eq!(u.assignees, None);
    }

    #[test]
    fn status_blocked_projects_to_closed_false_reopened() {
        // Status=Blocked is an OPEN status on our side, so the remote
        // projection is closed=false with Reopened (lifecycle_to_remote_state).
        // Guards the non-Done branch of the helper's status projection.
        let patch = MirrorPatch {
            status: Some(TaskStatus::Blocked),
            ..Default::default()
        };
        let u = build_update_from_patch(&any_task(), &patch, "github.com/o/r", "1")
            .expect("patch has status ⇒ Some");
        assert_eq!(u.closed, Some(false));
        assert_eq!(u.state_reason, Some(RemoteStateReason::Reopened));
    }

    #[test]
    fn assignees_some_passthrough() {
        // The patch carries assignees ⇒ the helper forwards them as
        // Some(&[..]). Three-state semantics (None omit / Some(&[]) clear /
        // Some(&[..]) set) are the adapter's job, not the helper's — the
        // helper just preserves the Option<&[String]> shape end-to-end.
        let patch = MirrorPatch {
            assignees: Some(vec!["alice".into(), "bob".into()]),
            ..Default::default()
        };
        let u = build_update_from_patch(&any_task(), &patch, "github.com/o/r", "1")
            .expect("patch has assignees ⇒ Some");
        assert_eq!(u.title, None);
        assert_eq!(u.body, None);
        assert_eq!(u.closed, None);
        assert_eq!(u.state_reason, None);
        assert_eq!(
            u.assignees,
            Some(&["alice".to_string(), "bob".to_string()][..])
        );
    }

    #[test]
    fn body_only_projects_only_body() {
        // A body-only diff: body is the only Some(_), everything else None.
        let patch = MirrorPatch {
            body: Some("revised body".into()),
            ..Default::default()
        };
        let u = build_update_from_patch(&any_task(), &patch, "github.com/o/r", "1")
            .expect("patch has body ⇒ Some");
        assert_eq!(u.title, None);
        assert_eq!(u.body, Some("revised body"));
        assert_eq!(u.closed, None);
        assert_eq!(u.state_reason, None);
        assert_eq!(u.assignees, None);
    }

    #[test]
    fn assignees_empty_some_is_clear() {
        // The clear branch of the three-state assignees semantics
        // (rpl-ffv). A `Some(vec![])` is NOT the same as `None`:
        // - `Some(&[])` → PATCH `"assignees": []` (clears all)
        // - `None`     → field omitted from PATCH (leaves unchanged)
        // The helper must forward `Some(vec![])` as `Some(&[])` — not
        // collapse to `None`. Locks the helper-layer invariant; the
        // adapter wire shape is already tested in
        // `update_issue_sets_and_clears_assignees` (infra-github).
        let patch = MirrorPatch {
            assignees: Some(vec![]),
            ..Default::default()
        };
        let u = build_update_from_patch(&any_task(), &patch, "github.com/o/r", "1")
            .expect("patch has assignees ⇒ Some");
        assert_eq!(u.title, None);
        assert_eq!(u.body, None);
        assert_eq!(u.closed, None);
        assert_eq!(u.state_reason, None);
        assert_eq!(
            u.assignees,
            Some(&[][..]),
            "Some(empty) is forwarded as Some(empty), NOT collapsed to None"
        );
    }

    #[test]
    fn status_archived_projects_to_closed_not_planned() {
        // Archived is the lifecycle's "we're not doing this" terminal
        // state; the remote projection is closed=true with
        // state_reason=NotPlanned (per `lifecycle_to_remote_state`).
        // Symmetric with the Done and Blocked helper tests; pins the
        // contract locally. The service tier has
        // `push_archived_task_closes_remote_with_not_planned` for the
        // end-to-end version.
        let patch = MirrorPatch {
            status: Some(TaskStatus::Archived),
            ..Default::default()
        };
        let u = build_update_from_patch(&any_task(), &patch, "github.com/o/r", "1")
            .expect("patch has status ⇒ Some");
        assert_eq!(u.title, None);
        assert_eq!(u.body, None);
        assert_eq!(u.closed, Some(true));
        assert_eq!(u.state_reason, Some(RemoteStateReason::NotPlanned));
        assert_eq!(u.assignees, None);
    }
}
