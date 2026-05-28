//! Free helpers for building [`SyncSummaryDto`]s and the small predicate
//! functions shared by [`crate::service::SyncService`].

use domain_sync::SyncDecision;
use domain_task::{SyncState, Task, TaskSnapshot, TaskStatus, assignees_equal};
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
/// aligned baseline. Status/labels are deliberately excluded because the pull
/// path doesn't copy them onto the local task (status' open/closed bit
/// doesn't map cleanly onto our 5-state lifecycle; the `Task` struct has no
/// label field). Comparing only mirrored fields prevents `updated_at` churn
/// — comments, reactions, label edits — from forcing cosmetic pull_remote
/// refreshes.
pub(crate) fn remote_mirrors_baseline(snap: &RemoteTaskSnapshot, baseline: &TaskSnapshot) -> bool {
    snap.title == baseline.title
        && snap.body == baseline.body
        // Order-insensitive: GitHub doesn't guarantee a stable assignee
        // ordering, and the domain's reconcile uses set equality too, so
        // matching on the same rule keeps the two views consistent.
        && assignees_equal(&snap.assignees, &baseline.assignees)
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
