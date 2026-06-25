//! application-sync — orchestrates remote promotion / push / pull + the
//! outbound outbox drainer.
//!
//! Local SQLite is authoritative for draft state; once a task has been
//! pushed, GitHub becomes the source of truth. Sync transitions follow
//! [`SyncState`]; lifecycle ([`Lifecycle`]) is orthogonal and only
//! consulted to skip closed/archived tasks.
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
use domain_task::{Lifecycle, MirrorPatch, Task, TaskSnapshot, assignees_equal};
use ports::{RemoteStateReason, RemoteTaskUpdate};

pub use drainer::{BackoffSchedule, OutboxDrainer};
pub use error::{Result, SyncError};
pub use poller::{PollReport, ProjectPoller};
pub use service::SyncService;

/// Resolve a task's open/closed bit to a project Status option id (RFC 0004
/// D1): an open task maps to one board option, a closed task to another.
/// Returns `None` when that bucket is unmapped (an option-less board) — the
/// caller then enqueues no `SetProjectStatus`.
///
/// Shared by the drainer's `AddItem` follow-up and the lifecycle / set-project
/// enqueue helpers so resolution is applied identically at enqueue time.
///
/// Delegates to the canonical [`Project::resolved_option_id_for`] so the
/// outbox enqueue/drain paths and Stage 8 drift detection (which calls the
/// domain resolver directly) can never diverge — there is exactly ONE
/// open/closed → option rule, and it lives in `domain-project`. Keeps the
/// `Option<String>` shape its callers expect by mapping the `&str`.
pub fn board_option_for_lifecycle(project: &Project, is_open: bool) -> Option<String> {
    project.resolved_option_id_for(is_open).map(str::to_string)
}

/// Decompose a [`Lifecycle`] into the remote issue's open/closed bit plus
/// `state_reason` (RFC 0004 D1): `Completed`/`NotPlanned` close with the
/// matching reason; `Reopened` is open with the closed→open marker; a fresh
/// `Open` is open with no reason (omitted from the PATCH).
///
/// Shared by [`SyncService::push`] and the [`OutboxDrainer`] so the two
/// outbound paths can never diverge on how a lifecycle change maps to the
/// remote.
pub(crate) fn lifecycle_to_remote_state(lifecycle: Lifecycle) -> (bool, Option<RemoteStateReason>) {
    // Decompose the single Lifecycle value into GitHub's two REST fields
    // `(closed, state_reason)` (RFC 0004 D1). `closed = !is_open`.
    match lifecycle {
        Lifecycle::Completed => (true, Some(RemoteStateReason::Completed)),
        Lifecycle::NotPlanned => (true, Some(RemoteStateReason::NotPlanned)),
        // Reopened carries the closed→open marker; a fresh Open carries no
        // reason (state_reason omitted from the PATCH).
        Lifecycle::Reopened => (false, Some(RemoteStateReason::Reopened)),
        Lifecycle::Open => (false, None),
    }
}

/// True iff the inbound mirror set of `(title, body, assignees)` matches the
/// `baseline`'s, using order-insensitive set equality for assignees
/// ([`assignees_equal`]). Used by `summary::remote_mirrors_baseline` and
/// (transitively) the pull/relink copy-back sites in `service::SyncService`.
///
/// The 3-field shape is the inbound mirror set per RFC 0003 §2 D7 —
/// **`Status` is deliberately absent** because pull can't map GitHub's
/// two-state open/closed onto the local lifecycle, and pulling
/// the REST closed bit back into the lifecycle is explicitly
/// out-of-scope per §3 of that RFC. The shape is encoded in this
/// helper's positional signature; the tripwires in `domain-task` and
/// `application-sync` re-assert the 3-field literal in their own test
/// mods so a divergence fails both build graphs.
///
/// Takes the three fields directly — not a `&RemoteTaskSnapshot` — so a
/// snapshot-struct field addition in `ports` cannot silently change the
/// projection. The `Status` exclusion (D7) is the explicit reason this
/// helper exists in parallel with [`MirrorField::differs`]: detection on
/// the issue-axis walks all four fields, but the inbound path excludes
/// `Status` because we cannot faithfully invert the open/closed bit.
pub(crate) fn inbound_mirrors_baseline(
    title: &str,
    body: &str,
    assignees: &[String],
    baseline: &TaskSnapshot,
) -> bool {
    title == baseline.title
        && body == baseline.body
        && assignees_equal(assignees, &baseline.assignees)
}

/// Copy the inbound mirror set (title, body, assignees) from a remote
/// snapshot onto a live `Task`. Single source of truth for the 3-field
/// copy shape — both the pull path and the relink path call this so a
/// future PR that adds a field to the inbound set gets a compile error
/// in **one** function signature, not two hand-rolled copy literals
/// that drift independently.
///
/// Direct field assignment (bypassing setters) is intentional: the
/// pull and relink paths both have already committed to the decision
/// that the remote content is authoritative for these fields (the
/// pull path went through `remote_mirrors_baseline` + `decide()` to
/// reach this point; the relink path overwrites unconditionally per
/// its verified-redirect contract). Re-running the per-field dirty
/// detection against the *old* baseline would be a self-defeating
/// no-op. `Status` is deliberately NOT copied (D7: pull can't
/// faithfully map GitHub's two-state open/closed onto the local
/// lifecycle).
pub(crate) fn copy_inbound_mirror_from_snap(
    task: &mut Task,
    remote_title: &str,
    remote_body: &str,
    remote_assignees: &[String],
) {
    task.title = remote_title.to_string();
    task.body = remote_body.to_string();
    task.assignees = remote_assignees.to_vec();
}

/// Project a [`MirrorPatch`] onto a [`RemoteTaskUpdate`]. Returns
/// `None` when the patch is empty so the caller skips the remote PATCH
/// entirely. Status in patch projects via [`lifecycle_to_remote_state`];
/// assignees pass through verbatim (the three-state `None`/`Some(&[])`/
/// `Some(&[..])` semantics live in the adapter).
///
/// Draft-backed mirrors are structurally unreachable: the planner
/// emits `UpdateRemote` only for `is_issue_backed` tasks (draft-backed
/// ones route to `UpdateDraftIssue`); push's `NoRemote` guard rejects
/// `task.remote.is_none()` upfront.
pub(crate) fn build_update_from_patch<'a>(
    _task: &Task,
    patch: &'a MirrorPatch,
    canonical_repo: &'a str,
    remote_id: &'a str,
) -> Option<RemoteTaskUpdate<'a>> {
    if patch.is_empty() {
        return None;
    }
    // Tie `closed` and `state_reason` together: both come through
    // when status in patch, both None when not. Survives a future
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
        // A status-only diff: patch.status = Completed ⇒ helper runs
        // lifecycle_to_remote_state, gets (true, Some(Completed)) ⇒ sets
        // closed=Some(true) and state_reason=Some(Completed). Title/body/
        // assignees stay None (status is the only changed field).
        let patch = MirrorPatch {
            status: Some(Lifecycle::Completed),
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
    fn status_reopened_projects_to_closed_false_reopened() {
        // Lifecycle::Reopened is an OPEN status on our side, so the remote
        // projection is closed=false with Reopened (lifecycle_to_remote_state).
        // Guards the non-Completed branch of the helper's status projection.
        let patch = MirrorPatch {
            status: Some(Lifecycle::Reopened),
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
        // Three-state: `Some(vec![])` = clear, `None` = omit. Wire
        // shape covered in `infra-github`.
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
        assert_eq!(u.assignees, Some(&[][..]));
    }

    #[test]
    fn status_not_planned_projects_to_closed_not_planned() {
        // NotPlanned → (true, NotPlanned) per `lifecycle_to_remote_state`.
        let patch = MirrorPatch {
            status: Some(Lifecycle::NotPlanned),
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

#[cfg(test)]
mod inbound_mirror_tests {
    use super::*;
    use domain_task::{MIRRORED_FIELDS, MirrorField};

    /// Tripwire for the D7 inbound carve-out (RFC 0003 §2 D7). Mirrors the
    /// tripwire in `domain-task::task::tests::inbound_mirror_set_excludes_status_per_d7`:
    /// if either crate's enumeration of the inbound set changes without
    /// the other, both build graphs fail. The duplication is the assertion.
    #[test]
    fn inbound_mirror_field_set_excludes_status() {
        // Tripwire for the D7 inbound carve-out (RFC 0003 §2 D7). The
        // inbound path excludes `Status` per D7 — the canonical
        // `MIRRORED_FIELDS` set is `{Title, Body, Status, Assignees}`,
        // the inbound subset is `{Title, Body, Assignees}`. Two
        // assertions pin both halves of the property so a future PR
        // that adds a new `MirrorField` (canonical growth) or removes
        // an inbound field (carve-out drift) fails the test.
        //
        // The `MIRRORED_FIELDS.len() == 4` half catches canonical-set
        // expansion (e.g. adding `MirrorField::Labels` would push the
        // canonical set to 5 and force a deliberate decision about
        // whether the new field is inbound or not). The
        // `MIRRORED_FIELDS.contains(&Status)` half pins the property
        // "Status is canonical but excluded from inbound" — a
        // hand-rolled `INBOUND` slice without `Status` is necessary
        // but not sufficient; this assertion makes it explicit.
        const INBOUND: [MirrorField; 3] = [
            MirrorField::Title,
            MirrorField::Body,
            MirrorField::Assignees,
        ];
        assert_eq!(
            MIRRORED_FIELDS.len(),
            4,
            "canonical MIRRORED_FIELDS must be exactly 4 fields (Title, Body, Status, Assignees)"
        );
        for f in INBOUND {
            assert!(
                MIRRORED_FIELDS.contains(&f),
                "inbound field {f:?} not in canonical MIRRORED_FIELDS"
            );
        }
        assert!(
            MIRRORED_FIELDS.contains(&MirrorField::Status),
            "Status is canonical but excluded from inbound — D7 carve-out, not a missing field"
        );
        assert!(
            !INBOUND.contains(&MirrorField::Status),
            "Status must remain outbound-only (D7)"
        );
    }

    #[test]
    fn inbound_mirrors_baseline_matches_per_field() {
        // Build a baseline snapshot via `Task::snapshot_view` so all the
        // non-mirrored bookkeeping fields (sync_state, priority, remote,
        // repo_id, filing_repo_id, captured_at) are filled in correctly.
        // The helper reads only title/body/assignees from the snapshot; the
        // rest is noise that exists to make the type usable end-to-end.
        let mut baseline_task =
            domain_task::Task::new_draft(domain_core::WorkspaceId::new(), None, "t".into())
                .unwrap();
        baseline_task.body = "b".into();
        baseline_task.assignees = vec!["alice".into(), "bob".into()];
        let snap = baseline_task.snapshot_view(domain_task::SnapshotSource::Pull);

        assert!(inbound_mirrors_baseline(
            "t",
            "b",
            &["alice".into(), "bob".into()],
            &snap
        ));
        // Title differs
        assert!(!inbound_mirrors_baseline(
            "T",
            "b",
            &["alice".into(), "bob".into()],
            &snap
        ));
        // Body differs
        assert!(!inbound_mirrors_baseline(
            "t",
            "B",
            &["alice".into(), "bob".into()],
            &snap
        ));
        // Assignees differ (different set)
        assert!(!inbound_mirrors_baseline(
            "t",
            "b",
            &["alice".into()],
            &snap
        ));
        // Assignees reorder: still equal (order-insensitive set eq)
        assert!(inbound_mirrors_baseline(
            "t",
            "b",
            &["bob".into(), "alice".into()],
            &snap
        ));
        // D7 carve-out is structurally guaranteed by the helper's
        // signature (no `status` parameter); the tripwire test
        // `inbound_mirror_field_set_excludes_status` pins it. Mutating
        // `baseline.status` and re-asserting here was vacuous — the
        // helper has no way to consult a field it doesn't take.
    }
}
