use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceOverview {
    pub workspace_id: String,
    pub workspace_name: String,
    pub workspace_status: String,
    pub repo_count: usize,
    pub worktree_count: usize,
    pub stale_worktree_count: usize,
    /// Task counts grouped by lifecycle status.
    pub by_status: BTreeMap<String, usize>,
    /// Task counts grouped by sync state.
    pub by_sync: BTreeMap<String, usize>,
    pub unsynced_task_count: usize,
    pub generated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedTaskRow {
    pub task_id: String,
    pub title: String,
    pub priority: String,
    pub blocked_by: Vec<String>,
}

/// One child of a parent task, as surfaced by `rl query children`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildTaskRow {
    pub task_id: String,
    pub title: String,
    pub status: String,
}

/// Completion rollup for a parent task's children. The `done`/`total` counts
/// are the aggregate the command exists to provide; `children` carries the
/// per-child detail (incomplete first, then by title).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildrenRollup {
    pub parent_id: String,
    pub total: usize,
    pub done: usize,
    pub children: Vec<ChildTaskRow>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleWorktreeRow {
    pub repo_id: String,
    pub canonical_url: String,
    pub path: String,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsyncedTaskRow {
    pub task_id: String,
    pub title: String,
    pub sync_state: String,
    /// Pending (local-only) outbound comments awaiting `sync push`. A task can
    /// be `Synced` on the snapshot axis yet still appear here with a non-zero
    /// count — comments are a separate outbound axis.
    pub pending_comments: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributorRow {
    pub assignee: String,
    pub total: usize,
    pub by_status: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyTaskRow {
    pub task_id: String,
    pub title: String,
    pub status: String,
    pub sync_state: String,
    pub priority: String,
    pub assignees: Vec<String>,
    /// Local issue type (RFC 0006, #236), e.g. `"bug"` / `"Story"`. `None` when
    /// the task carries no type. Local-only — never a network read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_type: Option<String>,
    /// CURRENT cached board status display name (e.g. `"Ready"`) — the same
    /// cached-status resolution `drift` uses (`project_status_option_id` →
    /// `Project::option_name_for`). `None` when projectless or not yet polled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_status: Option<String>,
}

/// One node of a workspace's ready frontier tree. A node is a task: it is
/// itself ready (`ready: true` — the actionable fields are populated), or it
/// is a container heading (`ready: false` — an open/closed parent whose
/// descendants include ready work, shown so the tree keeps its shape). Ready
/// children nest recursively under their parent node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyNode {
    pub task_id: String,
    pub title: String,
    pub ready: bool,
    /// Lifecycle of a ready node ("open"/"in_progress"). `None` for a pure
    /// container heading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Priority of a ready node. `None` for a pure container heading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// Assignees of a ready node. Empty for a pure container heading.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ReadyNode>,
}

/// One workspace's ready frontier as a tree: roots are top-level tasks
/// (parentless, or whose parent is outside this workspace / not in the
/// frontier); ready children nest under their parent node. Every level is
/// ordered by priority (best task first), then title.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyWorkspace {
    pub workspace_id: String,
    pub workspace_name: String,
    pub tree: Vec<ReadyNode>,
}

/// The ready frontier, grouped per workspace (and, within it, recursively by
/// parent task). The caller supplies the workspace scope; the array holds one
/// entry per workspace in scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyView {
    pub workspaces: Vec<ReadyWorkspace>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignedTaskRow {
    pub task_id: String,
    pub title: String,
    pub status: String,
    pub sync_state: String,
    pub priority: String,
    pub blocked: bool,
    pub remote_id: Option<String>,
    /// Local issue type (RFC 0006, #236), e.g. `"bug"` / `"Story"`. `None` when
    /// the task carries no type. Local-only — never a network read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_type: Option<String>,
    /// CURRENT cached board status display name (e.g. `"Ready"`) — the same
    /// cached-status resolution `drift` uses (`project_status_option_id` →
    /// `Project::option_name_for`). `None` when projectless or not yet polled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_status: Option<String>,
}

/// What `query drift` returns: the rows, plus any notice about how they were
/// produced.
///
/// An object rather than a bare array because a live read can be partial — some
/// rows fresh, others fallen back to cache — and that qualification has to
/// travel with the data instead of only reaching a human on stderr.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftReport {
    pub rows: Vec<DriftRow>,
    /// Empty on the happy path (every row live, or `--offline` as asked).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<QueryNoticeDto>,
}

/// How [`crate::QueryService::drift_report`] should source board status.
///
/// Three states, not two: asking for `--offline` and being unable to go live
/// both end at the cache, but only the second is a surprise the caller has to
/// be told about.
pub enum LiveRead<'a> {
    /// Cached only, because that is what was asked for. Emits no notice.
    Offline,
    /// Live was wanted and could not happen — no token, an unreadable token
    /// file, a provider that would not build. Reported as a notice.
    Unavailable(String),
    /// Read the board through this provider.
    Provider(&'a dyn ports::RemoteProjectProvider),
}

/// A user-facing notice attached to a query result. Internally tagged
/// (`"kind"`) so new kinds can be added without breaking the wire shape, and
/// structured only — the human-readable line is formatted by the CLI renderer,
/// so there is one source of truth for the prose. Same contract as
/// [`dto_shared::SyncNoticeDto`]; the CLI's `NoticeLine` trait is what lets one
/// renderer handle both.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueryNoticeDto {
    /// Some rows carry live board status and the rest fell back to the cache.
    /// Nothing on a row marks which is which — `last_refreshed_at` is the
    /// issue-sync timestamp and is untouched by either path — so the counts
    /// are all the report can honestly say.
    DriftPartiallyLive(DriftPartiallyLiveNotice),
    /// The live read could not run, so every row is cached. Distinct from
    /// `--offline`, which is the user asking for exactly this and needs no
    /// notice.
    DriftLiveUnavailable(DriftLiveUnavailableNotice),
    /// The live read worked but the fresh values could not be written back to
    /// the cache — most likely the daemon holding the database. The rows are
    /// still live and correct; only the next `--offline` run loses out.
    DriftCacheNotRefreshed(DriftCacheNotRefreshedNotice),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftPartiallyLiveNotice {
    pub live_count: usize,
    pub cached_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftLiveUnavailableNotice {
    /// Why the live read did not happen — a missing token, or the API error.
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftCacheNotRefreshedNotice {
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftRow {
    pub task_id: String,
    pub title: String,
    pub sync_state: String,
    pub remote_id: Option<String>,
    /// Which dimensions of this task have drifted. `"sync"` = the REST /
    /// local snapshot axis (`sync_state` diverged); `"project_status"` = the
    /// GitHub Projects v2 board status axis (the #39 axis). A row can carry
    /// either or both — the project-status axis is evaluated independently of
    /// `sync_state`, so a `Synced` task whose board moved still appears here
    /// with `reasons = ["project_status"]` and `sync_state = "synced"`.
    /// Additive; defaults to empty for older consumers.
    #[serde(default)]
    pub reasons: Vec<String>,
    /// The task's CURRENT cached remote project-board status, as a display
    /// name (e.g. `"Done"`). `None` when the task is projectless or hasn't
    /// been polled yet. This is the "actual" side of the project-status
    /// mismatch.
    #[serde(default)]
    pub project_status: Option<String>,
    /// The board status the task's local lifecycle status maps to, as a
    /// display name (e.g. `"In progress"`) — the "expected" side. `None` when
    /// projectless or unmappable. Compared against [`Self::project_status`]:
    /// when both are `Some` and differ, the project-status axis has drifted
    /// (the #39 acceptance: expected-vs-actual is explicit).
    #[serde(default)]
    pub project_status_expected: Option<String>,
    /// Wall-clock time the remote was last observed for this task (the
    /// write-through `synced_at`, RFC 0004 D2/D3). Lets a reader weigh a drift
    /// reason by freshness — a `project_status` drift with a 3-day-old stamp is
    /// qualitatively different from a 30-second-old one. OMITTED from the JSON
    /// (not `null`) when the task has never been observed. Additive; older
    /// consumers simply never see the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refreshed_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 0002 D5 / #119: the filing repo is an INTERNAL axis. None of the
    /// `QueryService` read-model rows are allowed to expose `filing_repo_id` —
    /// they stay on the logical axis only, so `jq` consumers and the rl-tasks
    /// skill keep working untouched. This asserts strictly the ABSENCE of the
    /// `filing_repo_id` key (most rows are task-id keyed and carry no repo
    /// column at all; only `StaleWorktreeRow` carries a logical `repo_id`).
    /// Adding a future row is a one-line addition to the `rows` array.
    #[test]
    fn read_models_json_omit_filing_repo_id() {
        let rows: Vec<(&str, serde_json::Value)> = vec![
            (
                "DriftRow",
                serde_json::to_value(DriftRow {
                    task_id: "rpl-1".into(),
                    title: "t".into(),
                    sync_state: "synced".into(),
                    remote_id: Some("1".into()),
                    reasons: vec!["sync".into()],
                    project_status: None,
                    project_status_expected: None,
                    last_refreshed_at: None,
                })
                .unwrap(),
            ),
            (
                "ReadyTaskRow",
                serde_json::to_value(ReadyTaskRow {
                    task_id: "rpl-2".into(),
                    title: "t".into(),
                    status: "open".into(),
                    sync_state: "local_only".into(),
                    priority: "p3".into(),
                    assignees: vec![],
                    issue_type: None,
                    project_status: None,
                })
                .unwrap(),
            ),
            (
                "AssignedTaskRow",
                serde_json::to_value(AssignedTaskRow {
                    task_id: "rpl-3".into(),
                    title: "t".into(),
                    status: "open".into(),
                    sync_state: "synced".into(),
                    priority: "p3".into(),
                    blocked: false,
                    remote_id: None,
                    issue_type: None,
                    project_status: None,
                })
                .unwrap(),
            ),
            (
                "UnsyncedTaskRow",
                serde_json::to_value(UnsyncedTaskRow {
                    task_id: "rpl-4".into(),
                    title: "t".into(),
                    sync_state: "dirty_local".into(),
                    pending_comments: 0,
                })
                .unwrap(),
            ),
            (
                "BlockedTaskRow",
                serde_json::to_value(BlockedTaskRow {
                    task_id: "rpl-5".into(),
                    title: "t".into(),
                    priority: "p3".into(),
                    blocked_by: vec![],
                })
                .unwrap(),
            ),
            (
                "ChildTaskRow",
                serde_json::to_value(ChildTaskRow {
                    task_id: "rpl-6".into(),
                    title: "t".into(),
                    status: "open".into(),
                })
                .unwrap(),
            ),
            (
                "ChildrenRollup",
                serde_json::to_value(ChildrenRollup {
                    parent_id: "rpl-7".into(),
                    total: 0,
                    done: 0,
                    children: vec![],
                })
                .unwrap(),
            ),
            (
                "ContributorRow",
                serde_json::to_value(ContributorRow {
                    assignee: "alice".into(),
                    total: 0,
                    by_status: BTreeMap::new(),
                })
                .unwrap(),
            ),
            (
                "StaleWorktreeRow",
                serde_json::to_value(StaleWorktreeRow {
                    repo_id: "repo-1".into(),
                    canonical_url: "https://example/repo".into(),
                    path: "/tmp/wt".into(),
                    status: "missing".into(),
                })
                .unwrap(),
            ),
            (
                "WorkspaceOverview",
                serde_json::to_value(WorkspaceOverview {
                    workspace_id: "ws-1".into(),
                    workspace_name: "ws".into(),
                    workspace_status: "active".into(),
                    repo_count: 0,
                    worktree_count: 0,
                    stale_worktree_count: 0,
                    by_status: BTreeMap::new(),
                    by_sync: BTreeMap::new(),
                    unsynced_task_count: 0,
                    generated_at: Utc::now(),
                })
                .unwrap(),
            ),
        ];

        for (name, v) in rows {
            let obj = v.as_object().expect("read-model row is a JSON object");
            assert!(
                !obj.contains_key("filing_repo_id"),
                "{name} JSON must NOT carry the internal filing_repo_id axis (RFC 0002 D5, #119)"
            );
        }
    }
}
