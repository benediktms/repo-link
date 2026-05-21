//! application-query — read-optimized views over the workspace.
//!
//! CQRS-light: each view returns a flat DTO shape ready for CLI rendering
//! or JSON output. No domain mutation lives here.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use domain_core::{IdParseError, WorkspaceId};
use domain_repo::LinkStatus;
use domain_task::TaskState;
use ports::{PortError, RepoBindingRepository, TaskFilter, TaskRepository, WorkspaceRepository};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum QueryError {
    #[error(transparent)]
    Port(#[from] PortError),
    #[error("invalid id: {0}")]
    BadId(String),
}

impl From<IdParseError> for QueryError {
    fn from(e: IdParseError) -> Self {
        Self::BadId(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, QueryError>;

// ---------- View DTOs ----------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceOverview {
    pub workspace_id: String,
    pub workspace_name: String,
    pub status: String,
    pub repo_count: usize,
    pub worktree_count: usize,
    pub stale_worktree_count: usize,
    pub task_states: BTreeMap<String, usize>,
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
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributorRow {
    pub assignee: String,
    pub total: usize,
    pub by_state: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyTaskRow {
    pub task_id: String,
    pub title: String,
    pub state: String,
    pub priority: String,
    pub assignees: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignedTaskRow {
    pub task_id: String,
    pub title: String,
    pub state: String,
    pub priority: String,
    pub blocked: bool,
    pub remote_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftRow {
    pub task_id: String,
    pub title: String,
    pub state: String,
    pub remote_id: Option<String>,
}

// ---------- QueryService -------------------------------------------------

pub struct QueryService {
    workspaces: Arc<dyn WorkspaceRepository>,
    bindings: Arc<dyn RepoBindingRepository>,
    tasks: Arc<dyn TaskRepository>,
}

impl QueryService {
    pub fn new(
        workspaces: Arc<dyn WorkspaceRepository>,
        bindings: Arc<dyn RepoBindingRepository>,
        tasks: Arc<dyn TaskRepository>,
    ) -> Self {
        Self {
            workspaces,
            bindings,
            tasks,
        }
    }

    pub async fn overview(&self, workspace_id: &str) -> Result<WorkspaceOverview> {
        let id: WorkspaceId = workspace_id.parse()?;
        let ws = self.workspaces.get(id).await?;
        let bindings = self.bindings.list_by_workspace(id).await?;
        let tasks = self
            .tasks
            .list(TaskFilter {
                workspace_id: Some(id),
                include_archived: false,
                ..TaskFilter::default()
            })
            .await?;

        let worktree_count: usize = bindings.iter().map(|b| b.worktrees.len()).sum();
        let stale_worktree_count: usize = bindings
            .iter()
            .flat_map(|b| b.worktrees.iter())
            .filter(|w| matches!(w.status, LinkStatus::Stale | LinkStatus::MissingPath))
            .count();

        let mut task_states: BTreeMap<String, usize> = BTreeMap::new();
        for t in &tasks {
            *task_states.entry(enum_str(&t.state)).or_insert(0) += 1;
        }
        let unsynced_task_count = tasks.iter().filter(|t| is_unsynced(t.state)).count();

        Ok(WorkspaceOverview {
            workspace_id: ws.id.to_string(),
            workspace_name: ws.name.as_str().to_string(),
            status: enum_str(&ws.status),
            repo_count: bindings.len(),
            worktree_count,
            stale_worktree_count,
            task_states,
            unsynced_task_count,
            generated_at: Utc::now(),
        })
    }

    pub async fn blocked_tasks(&self, workspace_id: &str) -> Result<Vec<BlockedTaskRow>> {
        let id: WorkspaceId = workspace_id.parse()?;
        let tasks = self
            .tasks
            .list(TaskFilter {
                workspace_id: Some(id),
                state: Some(TaskState::Blocked),
                ..TaskFilter::default()
            })
            .await?;
        Ok(tasks
            .iter()
            .map(|t| BlockedTaskRow {
                task_id: t.id.to_string(),
                title: t.title.clone(),
                priority: enum_str(&t.priority),
                blocked_by: t
                    .relations
                    .iter()
                    .filter(|r| r.kind == domain_task::RelationKind::BlockedBy)
                    .map(|r| r.other.to_string())
                    .collect(),
            })
            .collect())
    }

    pub async fn stale_worktrees(&self, workspace_id: &str) -> Result<Vec<StaleWorktreeRow>> {
        let id: WorkspaceId = workspace_id.parse()?;
        let bindings = self.bindings.list_by_workspace(id).await?;
        let mut out = Vec::new();
        for b in bindings {
            for w in &b.worktrees {
                if matches!(w.status, LinkStatus::Stale | LinkStatus::MissingPath) {
                    out.push(StaleWorktreeRow {
                        repo_id: b.id.to_string(),
                        canonical_url: b.canonical_url.clone(),
                        path: w.path.display().to_string(),
                        status: enum_str(&w.status),
                    });
                }
            }
        }
        Ok(out)
    }

    /// Open tasks assigned to `assignee` in this workspace.
    ///
    /// "Open" excludes only `Archived` for now. Remote-closed issues stay
    /// listed until they're pulled and archived locally — the daemon (G011)
    /// will close that loop. `blocked` is computed from BlockedBy relations,
    /// matching `ready_tasks` semantics.
    pub async fn assigned_to(
        &self,
        workspace_id: &str,
        assignee: &str,
    ) -> Result<Vec<AssignedTaskRow>> {
        use std::collections::HashMap;

        let id: WorkspaceId = workspace_id.parse()?;
        let tasks = self
            .tasks
            .list(TaskFilter {
                workspace_id: Some(id),
                include_archived: true,
                ..TaskFilter::default()
            })
            .await?;

        let by_id: HashMap<_, _> = tasks.iter().map(|t| (t.id, t)).collect();

        let mut rows: Vec<AssignedTaskRow> = tasks
            .iter()
            .filter(|t| t.state != TaskState::Archived)
            .filter(|t| t.assignees.iter().any(|a| a == assignee))
            .map(|t| {
                let blocked = t.relations.iter().any(|r| {
                    r.kind == domain_task::RelationKind::BlockedBy
                        && by_id
                            .get(&r.other)
                            .map(|other| other.state != TaskState::Archived)
                            .unwrap_or(false)
                });
                AssignedTaskRow {
                    task_id: t.id.to_string(),
                    title: t.title.clone(),
                    state: enum_str(&t.state),
                    priority: enum_str(&t.priority),
                    blocked,
                    remote_id: t.remote.as_ref().map(|r| r.remote_id.clone()),
                }
            })
            .collect();

        // Surface unblocked + highest priority first, then by updated_at asc.
        rows.sort_by(|a, b| a.blocked.cmp(&b.blocked).then_with(|| a.priority.cmp(&b.priority)));
        Ok(rows)
    }

    /// Tasks ready to work on right now: open + actionable + not transitively
    /// blocked. A task is treated as "blocking" if it is still in the
    /// workspace and not yet `Archived` — so closing/archiving the blocker is
    /// what unlocks the dependents.
    ///
    /// Sorted by priority (P0 first), then by `updated_at` ascending so the
    /// oldest waiting work surfaces first.
    pub async fn ready_tasks(&self, workspace_id: &str) -> Result<Vec<ReadyTaskRow>> {
        use std::collections::HashMap;

        let id: WorkspaceId = workspace_id.parse()?;
        let tasks = self
            .tasks
            .list(TaskFilter {
                workspace_id: Some(id),
                include_archived: true, // need archived to evaluate blocker status
                ..TaskFilter::default()
            })
            .await?;

        let by_id: HashMap<_, _> = tasks.iter().map(|t| (t.id, t)).collect();

        let is_actionable = |t: &domain_task::Task| {
            matches!(
                t.state,
                TaskState::Draft
                    | TaskState::Staged
                    | TaskState::Pushed
                    | TaskState::Synced
                    | TaskState::DirtyLocal
                    | TaskState::DirtyRemote
            )
        };

        let is_open_blocker = |other: domain_core::TaskId| {
            by_id
                .get(&other)
                .map(|t| t.state != TaskState::Archived)
                .unwrap_or(false)
        };

        let mut ready: Vec<&domain_task::Task> = tasks
            .iter()
            .filter(|t| is_actionable(t))
            .filter(|t| {
                !t.relations
                    .iter()
                    .any(|r| r.kind == domain_task::RelationKind::BlockedBy && is_open_blocker(r.other))
            })
            .collect();

        // Sort: priority asc (P0 first), then updated_at asc.
        ready.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| a.updated_at.cmp(&b.updated_at))
        });

        Ok(ready
            .into_iter()
            .map(|t| ReadyTaskRow {
                task_id: t.id.to_string(),
                title: t.title.clone(),
                state: enum_str(&t.state),
                priority: enum_str(&t.priority),
                assignees: t.assignees.clone(),
            })
            .collect())
    }

    /// Group tasks by assignee. Tasks with no assignee land under "(unassigned)".
    pub async fn contributors(&self, workspace_id: &str) -> Result<Vec<ContributorRow>> {
        let id: WorkspaceId = workspace_id.parse()?;
        let tasks = self
            .tasks
            .list(TaskFilter {
                workspace_id: Some(id),
                ..TaskFilter::default()
            })
            .await?;

        use std::collections::HashMap;
        let mut buckets: HashMap<String, (usize, BTreeMap<String, usize>)> = HashMap::new();
        for t in &tasks {
            let state = enum_str(&t.state);
            let assignees: Vec<String> = if t.assignees.is_empty() {
                vec!["(unassigned)".into()]
            } else {
                t.assignees.clone()
            };
            for name in assignees {
                let entry = buckets.entry(name).or_default();
                entry.0 += 1;
                *entry.1.entry(state.clone()).or_insert(0) += 1;
            }
        }

        let mut rows: Vec<ContributorRow> = buckets
            .into_iter()
            .map(|(assignee, (total, by_state))| ContributorRow {
                assignee,
                total,
                by_state,
            })
            .collect();
        rows.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.assignee.cmp(&b.assignee)));
        Ok(rows)
    }

    /// Tasks that have diverged from the remote (DirtyLocal/DirtyRemote/Conflict).
    /// This is the subset of unsynced tasks that need a reconciliation action.
    pub async fn drift(&self, workspace_id: &str) -> Result<Vec<DriftRow>> {
        let id: WorkspaceId = workspace_id.parse()?;
        let tasks = self
            .tasks
            .list(TaskFilter {
                workspace_id: Some(id),
                ..TaskFilter::default()
            })
            .await?;
        Ok(tasks
            .iter()
            .filter(|t| {
                matches!(
                    t.state,
                    TaskState::DirtyLocal | TaskState::DirtyRemote | TaskState::Conflict
                )
            })
            .map(|t| DriftRow {
                task_id: t.id.to_string(),
                title: t.title.clone(),
                state: enum_str(&t.state),
                remote_id: t.remote.as_ref().map(|r| r.remote_id.clone()),
            })
            .collect())
    }

    pub async fn unsynced_tasks(&self, workspace_id: &str) -> Result<Vec<UnsyncedTaskRow>> {
        let id: WorkspaceId = workspace_id.parse()?;
        let tasks = self
            .tasks
            .list(TaskFilter {
                workspace_id: Some(id),
                ..TaskFilter::default()
            })
            .await?;
        Ok(tasks
            .iter()
            .filter(|t| is_unsynced(t.state))
            .map(|t| UnsyncedTaskRow {
                task_id: t.id.to_string(),
                title: t.title.clone(),
                state: enum_str(&t.state),
            })
            .collect())
    }
}

// ---------- Helpers -------------------------------------------------------

fn enum_str<T: serde::Serialize>(t: &T) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

fn is_unsynced(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Draft
            | TaskState::Staged
            | TaskState::DirtyLocal
            | TaskState::DirtyRemote
            | TaskState::Conflict
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_repo::RepoBinding;
    use domain_task::Task;
    use domain_workspace::{Workspace, WorkspaceName};
    use std::path::PathBuf;
    use testing_fixtures::{
        InMemoryRepoBindingRepository, InMemoryTaskRepository, InMemoryWorkspaceRepository,
    };

    fn svc() -> (
        QueryService,
        Arc<InMemoryWorkspaceRepository>,
        Arc<InMemoryRepoBindingRepository>,
        Arc<InMemoryTaskRepository>,
    ) {
        let w = Arc::new(InMemoryWorkspaceRepository::new());
        let b = Arc::new(InMemoryRepoBindingRepository::new());
        let t = Arc::new(InMemoryTaskRepository::new());
        let svc = QueryService::new(w.clone(), b.clone(), t.clone());
        (svc, w, b, t)
    }

    #[tokio::test]
    async fn overview_counts_states_and_stale_worktrees() {
        let (svc, ws, bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let workspace_id = workspace.id;
        ws.save(&workspace).await.unwrap();

        let mut b = RepoBinding::new(
            workspace_id,
            "git@github.com:o/r.git".into(),
            "github.com/o/r".into(),
        )
        .unwrap();
        b.link_worktree(PathBuf::from("/tmp/a"), None);
        b.link_worktree(PathBuf::from("/tmp/b"), None);
        b.mark_path_missing(std::path::Path::new("/tmp/b")).unwrap();
        bs.save(&b).await.unwrap();

        let draft = Task::new_draft(workspace_id, None, "draft thing".into()).unwrap();
        let mut staged = Task::new_draft(workspace_id, None, "staged thing".into()).unwrap();
        staged.stage_for_sync().unwrap();
        ts.save(&draft).await.unwrap();
        ts.save(&staged).await.unwrap();

        let ov = svc.overview(&workspace_id.to_string()).await.unwrap();
        assert_eq!(ov.repo_count, 1);
        assert_eq!(ov.worktree_count, 2);
        assert_eq!(ov.stale_worktree_count, 1);
        assert_eq!(ov.task_states.get("draft"), Some(&1));
        assert_eq!(ov.task_states.get("staged"), Some(&1));
        assert_eq!(ov.unsynced_task_count, 2);
    }

    #[tokio::test]
    async fn blocked_tasks_view_includes_relation_ids() {
        let (svc, ws, _bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        let other = Task::new_draft(wid, None, "blocker".into()).unwrap();
        let mut blocked = Task::new_draft(wid, None, "the work".into()).unwrap();
        blocked.add_relation(domain_task::RelationKind::BlockedBy, other.id);
        blocked.mark_blocked();
        ts.save(&other).await.unwrap();
        ts.save(&blocked).await.unwrap();

        let rows = svc.blocked_tasks(&wid.to_string()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].blocked_by, vec![other.id.to_string()]);
    }

    #[tokio::test]
    async fn contributors_view_groups_and_sorts() {
        let (svc, ws, _bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        let mut a = Task::new_draft(wid, None, "a".into()).unwrap();
        a.assignees = vec!["alice".into(), "bob".into()];
        let mut b = Task::new_draft(wid, None, "b".into()).unwrap();
        b.assignees = vec!["alice".into()];
        let c = Task::new_draft(wid, None, "c".into()).unwrap(); // unassigned
        ts.save(&a).await.unwrap();
        ts.save(&b).await.unwrap();
        ts.save(&c).await.unwrap();

        let rows = svc.contributors(&wid.to_string()).await.unwrap();
        let alice = rows.iter().find(|r| r.assignee == "alice").unwrap();
        assert_eq!(alice.total, 2);
        assert_eq!(alice.by_state.get("draft"), Some(&2));
        let bob = rows.iter().find(|r| r.assignee == "bob").unwrap();
        assert_eq!(bob.total, 1);
        let unassigned = rows.iter().find(|r| r.assignee == "(unassigned)").unwrap();
        assert_eq!(unassigned.total, 1);
        // alice (2) sorts before bob/unassigned (1 each).
        assert_eq!(rows[0].assignee, "alice");
    }

    #[tokio::test]
    async fn ready_tasks_excludes_blocked_and_archived() {
        let (svc, ws, _bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        // A is a blocker that's still open.
        let blocker_a = Task::new_draft(wid, None, "blocker a".into()).unwrap();
        // B is a blocker that's been archived → no longer blocks.
        let mut blocker_b = Task::new_draft(wid, None, "blocker b".into()).unwrap();
        blocker_b.archive().unwrap();

        let mut blocked_by_a = Task::new_draft(wid, None, "needs a".into()).unwrap();
        blocked_by_a.add_relation(domain_task::RelationKind::BlockedBy, blocker_a.id);

        let mut unblocked = Task::new_draft(wid, None, "freed up".into()).unwrap();
        unblocked.add_relation(domain_task::RelationKind::BlockedBy, blocker_b.id);
        unblocked.set_priority(domain_task::Priority::P0);

        let mut also_unblocked = Task::new_draft(wid, None, "low pri".into()).unwrap();
        also_unblocked.set_priority(domain_task::Priority::P3);

        for t in [&blocker_a, &blocker_b, &blocked_by_a, &unblocked, &also_unblocked] {
            ts.save(t).await.unwrap();
        }

        let rows = svc.ready_tasks(&wid.to_string()).await.unwrap();
        // blocked_by_a is excluded; blocker_a + unblocked + also_unblocked are ready.
        // (blocker_b is archived, so it's not actionable.)
        let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
        assert!(titles.contains(&"freed up"));
        assert!(titles.contains(&"low pri"));
        assert!(titles.contains(&"blocker a"));
        assert!(!titles.contains(&"needs a"));
        assert!(!titles.contains(&"blocker b"));
        // P0 ("freed up") sorts before P3 ("low pri").
        let freed_idx = titles.iter().position(|t| *t == "freed up").unwrap();
        let low_idx = titles.iter().position(|t| *t == "low pri").unwrap();
        assert!(freed_idx < low_idx);
    }

    #[tokio::test]
    async fn assigned_to_filters_by_assignee_and_flags_blocked() {
        let (svc, ws, _bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        let blocker = Task::new_draft(wid, None, "the gate".into()).unwrap();
        let mut mine_open = Task::new_draft(wid, None, "open".into()).unwrap();
        mine_open.assignees = vec!["benedikt".into()];
        let mut mine_blocked = Task::new_draft(wid, None, "blocked".into()).unwrap();
        mine_blocked.assignees = vec!["benedikt".into()];
        mine_blocked.add_relation(domain_task::RelationKind::BlockedBy, blocker.id);
        let mut someone_elses = Task::new_draft(wid, None, "not me".into()).unwrap();
        someone_elses.assignees = vec!["alice".into()];
        let mut mine_done = Task::new_draft(wid, None, "done".into()).unwrap();
        mine_done.assignees = vec!["benedikt".into()];
        mine_done.archive().unwrap();

        for t in [&blocker, &mine_open, &mine_blocked, &someone_elses, &mine_done] {
            ts.save(t).await.unwrap();
        }

        let rows = svc.assigned_to(&wid.to_string(), "benedikt").await.unwrap();
        let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, vec!["open", "blocked"]); // sorted unblocked-first, then by priority
        assert!(!rows[0].blocked);
        assert!(rows[1].blocked);
    }

    #[tokio::test]
    async fn drift_view_returns_only_divergent_states() {
        let (svc, ws, _bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        let draft = Task::new_draft(wid, None, "still drafting".into()).unwrap();
        let mut dirty = Task::new_draft(wid, None, "edited locally".into()).unwrap();
        dirty.stage_for_sync().unwrap();
        dirty
            .promote_to_remote(domain_task::RemoteRef {
                provider: "github".into(),
                remote_id: "42".into(),
            })
            .unwrap();
        dirty.mark_synced().unwrap();
        dirty.mark_dirty_local().unwrap();
        ts.save(&draft).await.unwrap();
        ts.save(&dirty).await.unwrap();

        let rows = svc.drift(&wid.to_string()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, "dirty_local");
        assert_eq!(rows[0].remote_id.as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn stale_worktrees_view_returns_missing_only() {
        let (svc, ws, bs, _ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        let mut b = RepoBinding::new(wid, "x".into(), "github.com/o/r".into()).unwrap();
        b.link_worktree(PathBuf::from("/tmp/x"), None);
        b.link_worktree(PathBuf::from("/tmp/y"), None);
        b.mark_path_missing(std::path::Path::new("/tmp/x")).unwrap();
        bs.save(&b).await.unwrap();

        let rows = svc.stale_worktrees(&wid.to_string()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "/tmp/x");
        assert_eq!(rows[0].status, "missing_path");
    }
}
