//! Free helpers for building [`SyncSummaryDto`]s and the small predicate
//! functions shared by [`crate::service::SyncService`].

use domain_sync::SyncDecision;
use domain_task::{SyncState, Task, TaskSnapshot, TaskStatus};
use dto_shared::{RemoteRefDto, SyncSummaryDto};
use ports::RemoteTaskSnapshot;
use serde::Serialize;

use crate::error::{Result, SyncError};

pub(crate) fn ensure_not_archived(task: &Task) -> Result<()> {
    if task.status == TaskStatus::Archived {
        Err(SyncError::Archived)
    } else {
        Ok(())
    }
}

/// Whether the remote snapshot's *mirrored* fields match the task's last
/// aligned baseline. Routes through the shared `inbound_mirrors_baseline`
/// helper so the inbound field set is a single named constant
/// (`INBOUND_MIRROR_FIELDS`) instead of a hand-rolled `&&` chain. The
/// `Status` exclusion (RFC 0003 §2 D7) is the explicit reason this helper
/// exists in parallel with `MirrorField::differs`: detection on the
/// issue-axis walks all four canonical fields, but the inbound path
/// excludes `Status` because pull cannot map GitHub's two-state
/// open/closed onto the local 5-state lifecycle. Comparing only mirrored
/// fields prevents `updated_at` churn — comments, reactions, label edits —
/// from forcing cosmetic pull_remote refreshes.
pub(crate) fn remote_mirrors_baseline(snap: &RemoteTaskSnapshot, baseline: &TaskSnapshot) -> bool {
    crate::inbound_mirrors_baseline(&snap.title, &snap.body, &snap.assignees, baseline)
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
    SyncSummaryDto {
        task_id: task.id.to_string(),
        previous_state: enum_str(&prev),
        new_state: enum_str(&task.sync),
        decision: enum_str(&decision),
        remote: task.remote.as_ref().map(|r| RemoteRefDto {
            provider: r.provider.clone(),
            remote_id: r.remote_id.clone(),
        }),
        note: None,
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
    use ports::RemoteTaskSnapshot;

    /// Build a baseline snapshot for the helper-under-test. The fields we
    /// care about (title, body, assignees) are the three inbound fields;
    /// status is set explicitly to make the `ignores_status` test's intent
    /// visible (the helper must NOT consult this field).
    fn baseline(status: TaskStatus) -> TaskSnapshot {
        let mut t = domain_task::Task::new_draft(
            domain_core::WorkspaceId::new(),
            None,
            "t".into(),
        )
        .unwrap();
        t.body = "b".into();
        t.assignees = vec!["alice".into(), "bob".into()];
        t.status = status;
        t.snapshot_view(SnapshotSource::Pull)
    }

    fn any_remote_snap(title: String, body: String, assignees: Vec<String>, closed: bool) -> RemoteTaskSnapshot {
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

    /// Build a `RemoteTaskSnapshot` whose `closed` differs from the baseline's
    /// open lifecycle. The drift check (D7) must IGNORE that — pulling
    /// open/closed back into the 5-state lifecycle is out of scope. If a
    /// future refactor folds `closed` into the comparison, this test fails
    /// and forces a re-think.
    #[test]
    fn remote_mirrors_baseline_ignores_status() {
        let base = baseline(TaskStatus::Open);
        let snap = any_remote_snap(
            base.title.clone(),
            base.body.clone(),
            base.assignees.clone(),
            // closed differs from the baseline's open lifecycle.
            true,
        );
        assert!(
            remote_mirrors_baseline(&snap, &base),
            "remote_mirrors_baseline must ignore closed (D7)"
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
        let base = baseline(TaskStatus::InProgress);
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
