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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftRow {
    pub task_id: String,
    pub title: String,
    pub sync_state: String,
    pub remote_id: Option<String>,
}
