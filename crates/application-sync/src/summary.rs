//! Free helpers for building [`SyncSummaryDto`]s and the small predicate
//! functions shared by [`crate::service::SyncService`].

use domain_sync::SyncDecision;
use domain_task::{Lifecycle, SyncState, Task, TaskSnapshot};
use dto_shared::{RemoteRefDto, SyncSummaryDto};
use ports::RemoteTaskSnapshot;
use serde::Serialize;

use crate::error::{Result, SyncError};

pub(crate) fn ensure_not_archived(task: &Task) -> Result<()> {
    if task.lifecycle == Lifecycle::NotPlanned {
        Err(SyncError::Archived)
    } else {
        Ok(())
    }
}

/// Whether the remote snapshot's *mirrored* fields match the task's last
/// aligned baseline. Routes through the shared `inbound_mirrors_baseline`
/// helper so the inbound field set is a single named function signature
/// instead of a hand-rolled `&&` chain. The inbound set is title / body /
/// assignees / open-closed: RFC 0004 D1 made `is_open` the 1:1 inverse of the
/// REST `closed` bit, so the open/closed axis is now reflected on pull
/// (reversing the RFC 0003 §2 D7 carve-out). Comparing only mirrored fields
/// still prevents `updated_at` churn — comments, reactions, label edits — from
/// forcing cosmetic pull_remote refreshes.
pub(crate) fn remote_mirrors_baseline(snap: &RemoteTaskSnapshot, baseline: &TaskSnapshot) -> bool {
    crate::inbound_mirrors_baseline(
        &snap.title,
        &snap.body,
        &snap.assignees,
        snap.closed,
        baseline,
    )
}

pub(crate) fn link_summary(
    task: &Task,
    prev: SyncState,
    decision: &str,
    note: Option<String>,
) -> SyncSummaryDto {
    SyncSummaryDto {
        task_id: task.id.to_string(),
        previous_state: enum_str(&prev),
        new_state: enum_str(&task.sync),
        decision: decision.to_string(),
        remote: task.remote.as_ref().map(|r| RemoteRefDto {
            provider: r.provider.clone(),
            remote_id: r.remote_id.clone(),
        }),
        note,
    }
}

pub(crate) fn summary(task: &Task, prev: SyncState, decision: SyncDecision) -> SyncSummaryDto {
    summary_with_note(task, prev, decision, None)
}

pub(crate) fn summary_with_note(
    task: &Task,
    prev: SyncState,
    decision: SyncDecision,
    note: Option<String>,
) -> SyncSummaryDto {
    SyncSummaryDto {
        task_id: task.id.to_string(),
        previous_state: enum_str(&prev),
        new_state: enum_str(&task.sync),
        decision: enum_str(&decision),
        remote: task.remote.as_ref().map(|r| RemoteRefDto {
            provider: r.provider.clone(),
            remote_id: r.remote_id.clone(),
        }),
        note,
    }
}

pub(crate) fn provider_label(canonical: &str) -> String {
    if canonical.starts_with("github.com/") {
        "github".into()
    } else {
        "remote".into()
    }
}

fn enum_str<T: Serialize>(t: &T) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_core::Timestamp;
    use domain_task::SnapshotSource;

    /// Build a baseline snapshot for the helper-under-test. The inbound fields
    /// are title, body, assignees, and the open/closed bit; lifecycle is set
    /// explicitly so the `reflects_status` test can flip the remote `closed`
    /// bit against a known baseline open state.
    fn baseline(lifecycle: Lifecycle) -> TaskSnapshot {
        let mut t = domain_task::Task::new_draft(domain_core::WorkspaceId::new(), None, "t".into())
            .unwrap();
        t.body = "b".into();
        t.assignees = vec!["alice".into(), "bob".into()];
        t.lifecycle = lifecycle;
        t.snapshot_view(SnapshotSource::Pull)
    }

    fn any_remote_snap(
        title: String,
        body: String,
        assignees: Vec<String>,
        closed: bool,
    ) -> RemoteTaskSnapshot {
        RemoteTaskSnapshot {
            remote_id: "1".into(),
            node_id: Some("node_1".into()),
            title,
            body,
            closed,
            updated_at: Timestamp::now(),
            assignees,
            labels: vec![],
        }
    }

    /// Tripwire: the drift check now REFLECTS the open/closed bit.
    /// RFC 0004 D1 made `is_open` the 1:1 inverse of the REST `closed` bit, so
    /// against an `Open` baseline a `closed=true` remote is drift while a
    /// `closed=false` remote is not. (This reverses the earlier D7 carve-out
    /// where `closed` was ignored.)
    #[test]
    fn remote_mirrors_baseline_reflects_status() {
        let base = baseline(Lifecycle::Open);
        let snap_closed = any_remote_snap(
            base.title.clone(),
            base.body.clone(),
            base.assignees.clone(),
            true,
        );
        let snap_open = any_remote_snap(
            base.title.clone(),
            base.body.clone(),
            base.assignees.clone(),
            false,
        );
        // Remote open matches the open baseline (no drift on the status axis).
        assert!(
            remote_mirrors_baseline(&snap_open, &base),
            "remote open vs local open must not be drift"
        );
        // Remote closed against an open baseline IS drift — the bug this fixes.
        assert!(
            !remote_mirrors_baseline(&snap_closed, &base),
            "remote closed vs local open must surface as drift"
        );
        // And: a real field change (title) must still trip the check.
        let snap_title_differs = any_remote_snap(
            "different".into(),
            base.body.clone(),
            base.assignees.clone(),
            false,
        );
        assert!(
            !remote_mirrors_baseline(&snap_title_differs, &base),
            "title change must still surface as drift"
        );
    }

    /// Sanity: the helper produces the same answer as the hand-rolled
    /// `&&` chain did before the refactor, for the canonical equal-field
    /// case. Guards against a typo in the helper body.
    #[test]
    fn remote_mirrors_baseline_agrees_on_equal_fields() {
        let base = baseline(Lifecycle::Reopened);
        let snap = any_remote_snap(
            base.title.clone(),
            base.body.clone(),
            base.assignees.clone(),
            false,
        );
        assert!(remote_mirrors_baseline(&snap, &base));

        // Reorder assignees — must still compare equal (order-insensitive).
        let snap_reordered = any_remote_snap(
            base.title.clone(),
            base.body.clone(),
            vec!["bob".into(), "alice".into()],
            false,
        );
        assert!(remote_mirrors_baseline(&snap_reordered, &base));
    }
}
