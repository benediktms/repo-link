use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use domain_core::{TaskId, WorkspaceId};
use domain_project::Project;
use domain_repo::LinkStatus;
use domain_task::{RelationKind, SyncState, Task, TaskStatus};
use ports::{
    ProjectRepository, RepoBindingRepository, TaskFilter, TaskRepository, WorkspaceRepository,
};

use crate::dto::{
    AssignedTaskRow, BlockedTaskRow, ChildTaskRow, ChildrenRollup, ContributorRow, DriftRow,
    ReadyTaskRow, StaleWorktreeRow, UnsyncedTaskRow, WorkspaceOverview,
};
use crate::error::Result;

pub struct QueryService {
    workspaces: Arc<dyn WorkspaceRepository>,
    bindings: Arc<dyn RepoBindingRepository>,
    tasks: Arc<dyn TaskRepository>,
    /// Resolves `task → workspace → project` so [`Self::drift`] can surface the
    /// GitHub Projects v2 status axis (RFC 0001 Stage 8, closes #39).
    projects: Arc<dyn ProjectRepository>,
}

impl QueryService {
    pub fn new(
        workspaces: Arc<dyn WorkspaceRepository>,
        bindings: Arc<dyn RepoBindingRepository>,
        tasks: Arc<dyn TaskRepository>,
        projects: Arc<dyn ProjectRepository>,
    ) -> Self {
        Self {
            workspaces,
            bindings,
            tasks,
            projects,
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

        let mut by_status: BTreeMap<String, usize> = BTreeMap::new();
        let mut by_sync: BTreeMap<String, usize> = BTreeMap::new();
        for t in &tasks {
            *by_status.entry(enum_str(&t.status)).or_insert(0) += 1;
            *by_sync.entry(enum_str(&t.sync)).or_insert(0) += 1;
        }
        let unsynced_task_count = tasks.iter().filter(|t| is_unsynced(t.sync)).count();

        Ok(WorkspaceOverview {
            workspace_id: ws.id.to_string(),
            workspace_name: ws.name.as_str().to_string(),
            workspace_status: enum_str(&ws.status),
            repo_count: bindings.len(),
            worktree_count,
            stale_worktree_count,
            by_status,
            by_sync,
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
                status: Some(TaskStatus::Blocked),
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

    /// Completion rollup for a parent task's children.
    ///
    /// `parent_id` must already be a canonical task UUID — friendly-ID
    /// resolution lives in `TaskService`, so the CLI resolves before calling.
    ///
    /// Children are gathered from both directions of the parent/child pair so
    /// the view is robust against legacy rows that predate auto-reciprocal
    /// edges: the parent's own `parent_of` edges, unioned with any task that
    /// carries a `child_of` edge back to the parent. The reverse scan is
    /// workspace-agnostic and the union is loaded by id, so a child related
    /// cross-repo (in another workspace) is still found.
    ///
    /// Archived children are omitted: a completion rollup tracks active work,
    /// so a dropped subtask neither inflates `total` nor counts as `done`.
    pub async fn children(&self, parent_id: &str) -> Result<ChildrenRollup> {
        let parent_uuid: TaskId = parent_id.parse()?;
        let parent = self.tasks.get(parent_uuid).await?;

        let mut child_ids: HashSet<TaskId> = parent
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::ParentOf)
            .map(|r| r.other)
            .collect();

        // Reverse direction: any task carrying `child_of` -> parent. Scanned
        // across all workspaces (`workspace_id: None`) so cross-repo children
        // aren't missed; archived rows are excluded (`include_archived` stays
        // false in the default filter).
        let all_tasks = self.tasks.list(TaskFilter::default()).await?;
        for t in &all_tasks {
            if t.relations
                .iter()
                .any(|r| r.kind == RelationKind::ChildOf && r.other == parent.id)
            {
                child_ids.insert(t.id);
            }
        }

        // A task is never its own child. The service rejects self-relations at
        // creation, but a legacy/corrupt row pointing back at the parent must
        // not inflate `total`/`done`.
        child_ids.remove(&parent.id);

        let mut children = Vec::with_capacity(child_ids.len());
        for id in child_ids {
            let c = self.tasks.get(id).await?;
            // Children reached via the parent's `parent_of` edges are loaded
            // unconditionally; drop archived ones here so the rollup matches
            // the archived-excluding reverse scan above.
            if c.status == TaskStatus::Archived {
                continue;
            }
            children.push(ChildTaskRow {
                task_id: c.id.to_string(),
                title: c.title.clone(),
                status: enum_str(&c.status),
            });
        }
        // Outstanding work first, completed (`done`) last, then by title, with
        // task_id as a final tie-breaker so the order is fully deterministic
        // (the source `child_ids` is a HashSet). The `done` predicate matches
        // the `done` count below so ordering and the rollup stay consistent.
        children.sort_by(|a, b| {
            let is_done = |s: &str| s == "done";
            is_done(&a.status)
                .cmp(&is_done(&b.status))
                .then_with(|| a.title.cmp(&b.title))
                .then_with(|| a.task_id.cmp(&b.task_id))
        });

        let total = children.len();
        let done = children.iter().filter(|c| c.status == "done").count();
        Ok(ChildrenRollup {
            parent_id: parent.id.to_string(),
            total,
            done,
            children,
        })
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

    /// Tasks assigned to `assignee` that aren't archived. Sorted unblocked-
    /// first, then by priority.
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
            .filter(|t| t.status != TaskStatus::Archived)
            .filter(|t| t.assignees.iter().any(|a| a == assignee))
            .map(|t| {
                let blocked = t.relations.iter().any(|r| {
                    r.kind == domain_task::RelationKind::BlockedBy
                        && by_id
                            .get(&r.other)
                            .map(|other| !is_done_or_archived(other.status))
                            .unwrap_or(false)
                });
                AssignedTaskRow {
                    task_id: t.id.to_string(),
                    title: t.title.clone(),
                    status: enum_str(&t.status),
                    sync_state: enum_str(&t.sync),
                    priority: enum_str(&t.priority),
                    blocked,
                    remote_id: t.remote.as_ref().map(|r| r.remote_id.clone()),
                }
            })
            .collect();

        rows.sort_by(|a, b| {
            a.blocked
                .cmp(&b.blocked)
                .then_with(|| a.priority.cmp(&b.priority))
        });
        Ok(rows)
    }

    /// Tasks ready to work on right now: status ∈ {Open, InProgress}, sync
    /// not in Conflict, and not transitively blocked by another non-done
    /// task. Sorted by priority (P0 first), then `updated_at` asc.
    pub async fn ready_tasks(&self, workspace_id: &str) -> Result<Vec<ReadyTaskRow>> {
        use std::collections::HashMap;

        let id: WorkspaceId = workspace_id.parse()?;
        let tasks = self
            .tasks
            .list(TaskFilter {
                workspace_id: Some(id),
                include_archived: true, // need them to evaluate blocker status
                ..TaskFilter::default()
            })
            .await?;

        let by_id: HashMap<_, _> = tasks.iter().map(|t| (t.id, t)).collect();

        let is_open_or_in_progress = |t: &domain_task::Task| {
            matches!(t.status, TaskStatus::Open | TaskStatus::InProgress)
                && t.sync != SyncState::Conflict
        };

        let mut ready: Vec<&domain_task::Task> = tasks
            .iter()
            .filter(|t| is_open_or_in_progress(t))
            .filter(|t| !is_transitively_blocked(t.id, &by_id))
            .collect();

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
                status: enum_str(&t.status),
                sync_state: enum_str(&t.sync),
                priority: enum_str(&t.priority),
                assignees: t.assignees.clone(),
            })
            .collect())
    }

    /// Group non-archived tasks by assignee with lifecycle-status counts.
    /// Tasks with no assignee land under "(unassigned)".
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
            let status = enum_str(&t.status);
            let assignees: Vec<String> = if t.assignees.is_empty() {
                vec!["(unassigned)".into()]
            } else {
                t.assignees.clone()
            };
            for name in assignees {
                let entry = buckets.entry(name).or_default();
                entry.0 += 1;
                *entry.1.entry(status.clone()).or_insert(0) += 1;
            }
        }

        let mut rows: Vec<ContributorRow> = buckets
            .into_iter()
            .map(|(assignee, (total, by_status))| ContributorRow {
                assignee,
                total,
                by_status,
            })
            .collect();
        rows.sort_by(|a, b| {
            b.total
                .cmp(&a.total)
                .then_with(|| a.assignee.cmp(&b.assignee))
        });
        Ok(rows)
    }

    /// Tasks whose local state has diverged from the remote, across two
    /// independent axes:
    ///
    /// 1. **Sync axis** — `sync_state ∈ {DirtyLocal, DirtyRemote, Conflict}`
    ///    (the REST / local snapshot diverged). Unchanged from before.
    /// 2. **Project-status axis (closes #39)** — the cached remote
    ///    GitHub Projects v2 board status disagrees with the option the task's
    ///    local lifecycle status maps to. This is evaluated **independently of
    ///    `sync_state`**, so a `Synced` task whose board card moved to "Done"
    ///    while REST still says open surfaces here.
    ///
    /// A task surfaces if either axis drifted; `reasons` names which. The
    /// project axis is only a mismatch when BOTH the cached actual
    /// (`project_status_option_id`) and the resolved expected option are
    /// `Some` and differ: a NULL cache means "not yet polled" (never flagged),
    /// a projectless task has no expected option (never flagged), and
    /// `Archived` is excluded from project mappings (never flagged).
    pub async fn drift(&self, workspace_id: &str) -> Result<Vec<DriftRow>> {
        let id: WorkspaceId = workspace_id.parse()?;
        let tasks = self
            .tasks
            .list(TaskFilter {
                workspace_id: Some(id),
                ..TaskFilter::default()
            })
            .await?;

        // One workspace per query, so its project (if any) resolves once.
        let project = self.resolve_workspace_project(id).await?;

        let mut rows = Vec::new();
        for t in &tasks {
            let sync_drift = matches!(
                t.sync,
                SyncState::DirtyLocal | SyncState::DirtyRemote | SyncState::Conflict
            );

            // Project-status axis, independent of sync_state.
            let (project_status, project_status_expected, project_drift) =
                project_axis(project.as_ref(), t);

            // rpl-sv2: a third axis that catches the silent divergence
            // where a task's recorded `filing_repo_id` references a
            // binding that's been deleted out from under it (e.g. an
            // org-move replaced the canonical binding with a new UUID
            // but never re-pointed the column — and there's no FK).
            // `Port(NotFound)` is the silent case; other errors
            // propagate. A `Synced` task with a dangling filing
            // binding is the load-bearing assertion (a `DirtyLocal`
            // / `Conflict` task would already show up on the sync
            // axis, but the filing axis is independently useful for
            // the common case where only the filing pointer broke).
            //
            // Cost: one PK lookup per task. For a workspace with ~60
            // affected tasks this is 60 extra lookups, all backed by
            // the `repos` PK index. If a future workspace ever has
            // thousands of tasks and the per-task probe becomes a hot
            // path, batch this via a `bindings.list_by_ids(&[uuid; n])`
            // port method — out of scope for the first cut.
            let filing_drift = match t.filing_repo_id {
                Some(filing_id) => matches!(
                    self.bindings.get(filing_id).await,
                    Err(ports::PortError::NotFound(_))
                ),
                None => false,
            };

            if !sync_drift && !project_drift && !filing_drift {
                continue;
            }

            let mut reasons = Vec::new();
            if sync_drift {
                reasons.push("sync".to_string());
            }
            if project_drift {
                reasons.push("project_status".to_string());
            }
            if filing_drift {
                reasons.push("filing_repo".to_string());
            }

            rows.push(DriftRow {
                task_id: t.id.to_string(),
                title: t.title.clone(),
                sync_state: enum_str(&t.sync),
                remote_id: t.remote.as_ref().map(|r| r.remote_id.clone()),
                reasons,
                project_status,
                project_status_expected,
            });
        }
        Ok(rows)
    }

    /// Resolve the workspace's parent project, if any. `None` for projectless
    /// workspaces (the common path) — the project-status drift axis is then
    /// inert for every task in the workspace.
    async fn resolve_workspace_project(&self, id: WorkspaceId) -> Result<Option<Project>> {
        let ws = self.workspaces.get(id).await?;
        let Some(project_id) = ws.project_id.clone() else {
            return Ok(None);
        };
        Ok(Some(self.projects.get(project_id).await?))
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
        // `list` skips comments, so fetch pending counts separately. A task is
        // unsynced if its snapshot axis is dirty OR it owes outbound comments.
        let pending = self.tasks.pending_comment_counts(id).await?;
        Ok(tasks
            .iter()
            .filter_map(|t| {
                let pending_comments = pending.get(&t.id).copied().unwrap_or(0);
                if !is_unsynced(t.sync) && pending_comments == 0 {
                    return None;
                }
                Some(UnsyncedTaskRow {
                    task_id: t.id.to_string(),
                    title: t.title.clone(),
                    sync_state: enum_str(&t.sync),
                    pending_comments,
                })
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

fn is_unsynced(sync: SyncState) -> bool {
    !matches!(sync, SyncState::Synced)
}

/// Evaluate the project-status drift axis for one task (RFC 0001 Stage 8,
/// closes #39). Returns `(actual_name, expected_name, is_drift)`:
///
/// - `actual_name`: the cached remote board status (`task.project_status_option_id`)
///   resolved to a display name. `None` when projectless or unpolled.
/// - `expected_name`: the option the task's local lifecycle status maps to
///   (via [`Project::resolved_option_id_for`], the SAME Blocked→Open fallback
///   the outbox enqueue path uses), resolved to a display name.
/// - `is_drift`: `true` only when BOTH the actual and expected option ids are
///   `Some` and differ. A NULL cache (`None` actual) is "not yet polled" — not
///   a mismatch. `Archived` resolves to no expected option, so it's never
///   flagged.
fn project_axis(project: Option<&Project>, task: &Task) -> (Option<String>, Option<String>, bool) {
    let Some(project) = project else {
        return (None, None, false);
    };

    let actual_id = task.project_status_option_id.as_deref();
    let expected_id = project.resolved_option_id_for(task.status);

    let actual_name = actual_id.and_then(|id| project.option_name_for(id).map(str::to_string));
    let expected_name = expected_id.and_then(|id| project.option_name_for(id).map(str::to_string));

    // Mismatch only when both sides are known and differ. A NULL cache
    // (`actual_id` None) is unpolled — never a mismatch.
    let is_drift = match (actual_id, expected_id) {
        (Some(a), Some(e)) => a != e,
        _ => false,
    };

    (actual_name, expected_name, is_drift)
}

fn is_done_or_archived(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Done | TaskStatus::Archived)
}

/// Whether `task_id` is blocked by any task reachable through a chain of
/// `BlockedBy` relations whose status is not `Done`/`Archived`.
///
/// DFS over `BlockedBy` edges only. A resolved (done/archived) blocker does not
/// block on its own, but the chain behind it is still followed — the contract
/// is "any *reachable* active blocker", so `A → B(done) → C(open)` leaves `A`
/// blocked. A relation cycle (`A ↔ A`, `A ↔ B`) is bounded by `visited`.
fn is_transitively_blocked(
    task_id: domain_core::TaskId,
    by_id: &std::collections::HashMap<domain_core::TaskId, &domain_task::Task>,
) -> bool {
    use std::collections::HashSet;

    let mut visited: HashSet<domain_core::TaskId> = HashSet::new();
    // Seed with the start task's direct blockers — the task's own status never
    // blocks itself, but a self-`BlockedBy` edge (re-)enqueues it below.
    let mut stack: Vec<domain_core::TaskId> = match by_id.get(&task_id) {
        Some(start) => start
            .relations
            .iter()
            .filter(|r| r.kind == domain_task::RelationKind::BlockedBy)
            .map(|r| r.other)
            .collect(),
        None => return false,
    };

    while let Some(current) = stack.pop() {
        if !visited.insert(current) {
            continue; // already explored — breaks cycles
        }
        match by_id.get(&current) {
            // An active blocker we can reach → blocked.
            Some(blocker) if !is_done_or_archived(blocker.status) => return true,
            // Resolved blocker: doesn't block, but keep following its chain.
            Some(blocker) => stack.extend(
                blocker
                    .relations
                    .iter()
                    .filter(|r| r.kind == domain_task::RelationKind::BlockedBy)
                    .map(|r| r.other),
            ),
            // Unknown id (e.g. archived-and-pruned) → treat as non-blocking.
            None => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_repo::RepoBinding;
    use domain_task::{RemoteRef, SnapshotSource, Task};
    use domain_workspace::{Workspace, WorkspaceName};
    use std::path::PathBuf;
    use testing_fixtures::{
        InMemoryProjectRepository, InMemoryRepoBindingRepository, InMemoryTaskRepository,
        InMemoryWorkspaceRepository,
    };

    fn svc() -> (
        QueryService,
        Arc<InMemoryWorkspaceRepository>,
        Arc<InMemoryRepoBindingRepository>,
        Arc<InMemoryTaskRepository>,
    ) {
        let (svc, w, b, t, _p) = svc_with_projects();
        (svc, w, b, t)
    }

    /// Like [`svc`] but also hands back the project repo so the Stage-8
    /// project-status drift tests can attach a project to a workspace.
    fn svc_with_projects() -> (
        QueryService,
        Arc<InMemoryWorkspaceRepository>,
        Arc<InMemoryRepoBindingRepository>,
        Arc<InMemoryTaskRepository>,
        Arc<InMemoryProjectRepository>,
    ) {
        let w = Arc::new(InMemoryWorkspaceRepository::new());
        let b = Arc::new(InMemoryRepoBindingRepository::new());
        let t = Arc::new(InMemoryTaskRepository::new());
        let p = Arc::new(InMemoryProjectRepository::new());
        let svc = QueryService::new(w.clone(), b.clone(), t.clone(), p.clone());
        (svc, w, b, t, p)
    }

    #[tokio::test]
    async fn overview_counts_status_sync_and_stale_worktrees() {
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

        let local_only = Task::new_draft(workspace_id, None, "still local".into()).unwrap();
        let mut staged = Task::new_draft(workspace_id, None, "staged thing".into()).unwrap();
        staged.stage_for_sync().unwrap();
        ts.save(&local_only, SnapshotSource::LocalEdit)
            .await
            .unwrap();
        ts.save(&staged, SnapshotSource::LocalEdit).await.unwrap();

        let ov = svc.overview(&workspace_id.to_string()).await.unwrap();
        assert_eq!(ov.repo_count, 1);
        assert_eq!(ov.worktree_count, 2);
        assert_eq!(ov.stale_worktree_count, 1);
        // Both tasks land in `Open` lifecycle status.
        assert_eq!(ov.by_status.get("open"), Some(&2));
        // But they differ in sync state.
        assert_eq!(ov.by_sync.get("local_only"), Some(&1));
        assert_eq!(ov.by_sync.get("staged"), Some(&1));
        assert_eq!(ov.unsynced_task_count, 2);
    }

    #[tokio::test]
    async fn unsynced_surfaces_synced_task_with_pending_comment() {
        let (svc, ws, _bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let workspace_id = workspace.id;
        ws.save(&workspace).await.unwrap();

        // A fully-synced task: clean on the snapshot axis.
        let mut t = Task::new_draft(workspace_id, None, "synced task".into()).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", "1")).unwrap();
        assert_eq!(t.sync, SyncState::Synced);
        ts.save(&t, SnapshotSource::Push).await.unwrap();

        // No outbound work yet → absent from unsynced.
        assert!(
            svc.unsynced_tasks(&workspace_id.to_string())
                .await
                .unwrap()
                .is_empty()
        );

        // A pending comment surfaces the task even though it stays `Synced`.
        ts.add_pending_comment(t.id, "me", "ping", domain_core::Timestamp::now())
            .await
            .unwrap();
        let rows = svc.unsynced_tasks(&workspace_id.to_string()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sync_state, "synced");
        assert_eq!(rows[0].pending_comments, 1);
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
        blocked.mark_blocked().unwrap();
        ts.save(&other, SnapshotSource::LocalEdit).await.unwrap();
        ts.save(&blocked, SnapshotSource::LocalEdit).await.unwrap();

        let rows = svc.blocked_tasks(&wid.to_string()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].blocked_by, vec![other.id.to_string()]);
    }

    #[tokio::test]
    async fn children_rollup_unions_both_directions_and_counts_done() {
        let (svc, ws, _bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        let mut parent = Task::new_draft(wid, None, "parent".into()).unwrap();

        // Direction 1: parent's own `parent_of` edge points to an open child.
        let child_open = Task::new_draft(wid, None, "open child".into()).unwrap();
        parent.add_relation(domain_task::RelationKind::ParentOf, child_open.id);

        // Direction 2: a *done* child points back via `child_of`, with no
        // matching `parent_of` on the parent — exercises the union scan.
        let mut child_done = Task::new_draft(wid, None, "done child".into()).unwrap();
        child_done.add_relation(domain_task::RelationKind::ChildOf, parent.id);
        child_done.start().unwrap();
        child_done.complete().unwrap();

        // An unrelated task in the same workspace must not leak in.
        let unrelated = Task::new_draft(wid, None, "unrelated".into()).unwrap();

        for t in [&parent, &child_open, &child_done, &unrelated] {
            ts.save(t, SnapshotSource::LocalEdit).await.unwrap();
        }

        let rollup = svc.children(&parent.id.to_string()).await.unwrap();
        assert_eq!(rollup.total, 2);
        assert_eq!(rollup.done, 1);
        // Incomplete sorts first, done sinks to the bottom.
        assert_eq!(rollup.children[0].task_id, child_open.id.to_string());
        assert_eq!(rollup.children[0].status, "open");
        assert_eq!(rollup.children[1].task_id, child_done.id.to_string());
        assert_eq!(rollup.children[1].status, "done");
    }

    #[tokio::test]
    async fn children_rollup_omits_archived_children() {
        let (svc, ws, _bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        let mut parent = Task::new_draft(wid, None, "parent".into()).unwrap();
        let active = Task::new_draft(wid, None, "active child".into()).unwrap();
        let mut archived = Task::new_draft(wid, None, "archived child".into()).unwrap();
        archived.archive().unwrap();
        parent.add_relation(domain_task::RelationKind::ParentOf, active.id);
        parent.add_relation(domain_task::RelationKind::ParentOf, archived.id);

        for t in [&parent, &active, &archived] {
            ts.save(t, SnapshotSource::LocalEdit).await.unwrap();
        }

        // Archived child is dropped from a completion rollup entirely: it
        // neither inflates `total` nor counts toward `done`.
        let rollup = svc.children(&parent.id.to_string()).await.unwrap();
        assert_eq!(rollup.total, 1);
        assert_eq!(rollup.done, 0);
        assert_eq!(rollup.children[0].task_id, active.id.to_string());
    }

    #[tokio::test]
    async fn children_rollup_excludes_self_reference() {
        // A corrupt self-referential edge must not make a task its own child.
        // The service rejects self-relations at creation, but a legacy row
        // could still carry one — the rollup must stay defensive.
        let (svc, ws, _bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        let mut parent = Task::new_draft(wid, None, "parent".into()).unwrap();
        parent.add_relation(domain_task::RelationKind::ParentOf, parent.id);
        ts.save(&parent, SnapshotSource::LocalEdit).await.unwrap();

        let rollup = svc.children(&parent.id.to_string()).await.unwrap();
        assert_eq!(rollup.total, 0, "a task must not be its own child");
    }

    #[tokio::test]
    async fn children_rollup_finds_cross_workspace_child() {
        let (svc, ws, _bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w1").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();
        // A second workspace — the parent does not know about its child here.
        let workspace2 = Workspace::new(WorkspaceName::new("w2").unwrap(), None, true);
        let wid2 = workspace2.id;
        ws.save(&workspace2).await.unwrap();

        let parent = Task::new_draft(wid, None, "parent".into()).unwrap();
        // The only link is the reverse `child_of` on a child living in another
        // workspace — discovery must not be scoped to the parent's workspace.
        // The child is `done`, so this also guards that the `done` rollup
        // counts a cross-workspace child, not just `total`.
        let mut cross = Task::new_draft(wid2, None, "cross-repo child".into()).unwrap();
        cross.add_relation(domain_task::RelationKind::ChildOf, parent.id);
        cross.start().unwrap();
        cross.complete().unwrap();

        ts.save(&parent, SnapshotSource::LocalEdit).await.unwrap();
        ts.save(&cross, SnapshotSource::LocalEdit).await.unwrap();

        let rollup = svc.children(&parent.id.to_string()).await.unwrap();
        assert_eq!(rollup.total, 1);
        assert_eq!(rollup.done, 1);
        assert_eq!(rollup.children[0].task_id, cross.id.to_string());
        assert_eq!(rollup.children[0].status, "done");
    }

    #[tokio::test]
    async fn contributors_view_groups_and_sorts_by_status() {
        let (svc, ws, _bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        let mut a = Task::new_draft(wid, None, "a".into()).unwrap();
        a.assignees = vec!["alice".into(), "bob".into()];
        let mut b = Task::new_draft(wid, None, "b".into()).unwrap();
        b.assignees = vec!["alice".into()];
        let c = Task::new_draft(wid, None, "c".into()).unwrap();
        ts.save(&a, SnapshotSource::LocalEdit).await.unwrap();
        ts.save(&b, SnapshotSource::LocalEdit).await.unwrap();
        ts.save(&c, SnapshotSource::LocalEdit).await.unwrap();

        let rows = svc.contributors(&wid.to_string()).await.unwrap();
        let alice = rows.iter().find(|r| r.assignee == "alice").unwrap();
        assert_eq!(alice.total, 2);
        assert_eq!(alice.by_status.get("open"), Some(&2));
        let bob = rows.iter().find(|r| r.assignee == "bob").unwrap();
        assert_eq!(bob.total, 1);
        let unassigned = rows.iter().find(|r| r.assignee == "(unassigned)").unwrap();
        assert_eq!(unassigned.total, 1);
        assert_eq!(rows[0].assignee, "alice");
    }

    #[tokio::test]
    async fn ready_tasks_excludes_blocked_and_archived() {
        let (svc, ws, _bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        let blocker_a = Task::new_draft(wid, None, "blocker a".into()).unwrap();
        let mut blocker_b = Task::new_draft(wid, None, "blocker b".into()).unwrap();
        blocker_b.archive().unwrap();

        let mut blocked_by_a = Task::new_draft(wid, None, "needs a".into()).unwrap();
        blocked_by_a.add_relation(domain_task::RelationKind::BlockedBy, blocker_a.id);

        let mut unblocked = Task::new_draft(wid, None, "freed up".into()).unwrap();
        unblocked.add_relation(domain_task::RelationKind::BlockedBy, blocker_b.id);
        unblocked.set_priority(domain_task::Priority::P0);

        let mut also_unblocked = Task::new_draft(wid, None, "low pri".into()).unwrap();
        also_unblocked.set_priority(domain_task::Priority::P3);

        for t in [
            &blocker_a,
            &blocker_b,
            &blocked_by_a,
            &unblocked,
            &also_unblocked,
        ] {
            ts.save(t, SnapshotSource::LocalEdit).await.unwrap();
        }

        let rows = svc.ready_tasks(&wid.to_string()).await.unwrap();
        let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
        assert!(titles.contains(&"freed up"));
        assert!(titles.contains(&"low pri"));
        assert!(titles.contains(&"blocker a"));
        assert!(!titles.contains(&"needs a"));
        assert!(!titles.contains(&"blocker b"));
        let freed_idx = titles.iter().position(|t| *t == "freed up").unwrap();
        let low_idx = titles.iter().position(|t| *t == "low pri").unwrap();
        assert!(freed_idx < low_idx);
    }

    /// Move a freshly-drafted task all the way to `Done` (the only legal path is
    /// `Open → InProgress → Done`).
    fn completed(mut t: Task) -> Task {
        t.start().unwrap();
        t.complete().unwrap();
        t
    }

    #[tokio::test]
    async fn ready_tasks_excludes_transitively_blocked() {
        // A → B(done) → C(open): the *direct* blocker B is resolved, so the old
        // one-hop check would wrongly mark A ready. The open tail C must keep A
        // out of the ready list.
        let (svc, ws, _bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        let c_open = Task::new_draft(wid, None, "c (open tail)".into()).unwrap();
        let mut b_done = Task::new_draft(wid, None, "b (done middle)".into()).unwrap();
        b_done.add_relation(domain_task::RelationKind::BlockedBy, c_open.id);
        let b_done = completed(b_done);
        let mut a = Task::new_draft(wid, None, "a (head)".into()).unwrap();
        a.add_relation(domain_task::RelationKind::BlockedBy, b_done.id);

        for t in [&c_open, &b_done, &a] {
            ts.save(t, SnapshotSource::LocalEdit).await.unwrap();
        }

        let titles: Vec<String> = svc
            .ready_tasks(&wid.to_string())
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert!(titles.iter().any(|t| t == "c (open tail)"));
        assert!(!titles.iter().any(|t| t == "a (head)"));
    }

    #[tokio::test]
    async fn ready_tasks_includes_fully_resolved_chain() {
        // A → B(done) → C(done): every reachable blocker is resolved, so A is
        // ready.
        let (svc, ws, _bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        let c_done = completed(Task::new_draft(wid, None, "c".into()).unwrap());
        let mut b_done = Task::new_draft(wid, None, "b".into()).unwrap();
        b_done.add_relation(domain_task::RelationKind::BlockedBy, c_done.id);
        let b_done = completed(b_done);
        let mut a = Task::new_draft(wid, None, "a".into()).unwrap();
        a.add_relation(domain_task::RelationKind::BlockedBy, b_done.id);

        for t in [&c_done, &b_done, &a] {
            ts.save(t, SnapshotSource::LocalEdit).await.unwrap();
        }

        let titles: Vec<String> = svc
            .ready_tasks(&wid.to_string())
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert!(titles.iter().any(|t| t == "a"));
    }

    #[tokio::test]
    async fn ready_tasks_handles_self_cycle() {
        // A blocked by itself: must terminate and report A as blocked.
        let (svc, ws, _bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        let mut a = Task::new_draft(wid, None, "a".into()).unwrap();
        a.add_relation(domain_task::RelationKind::BlockedBy, a.id);
        ts.save(&a, SnapshotSource::LocalEdit).await.unwrap();

        let rows = svc.ready_tasks(&wid.to_string()).await.unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn ready_tasks_handles_mutual_cycle() {
        // A ↔ B (each blocks the other): must terminate with both blocked.
        let (svc, ws, _bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        let mut a = Task::new_draft(wid, None, "a".into()).unwrap();
        let mut b = Task::new_draft(wid, None, "b".into()).unwrap();
        a.add_relation(domain_task::RelationKind::BlockedBy, b.id);
        b.add_relation(domain_task::RelationKind::BlockedBy, a.id);

        for t in [&a, &b] {
            ts.save(t, SnapshotSource::LocalEdit).await.unwrap();
        }

        let rows = svc.ready_tasks(&wid.to_string()).await.unwrap();
        assert!(rows.is_empty());
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
        let mut mine_archived = Task::new_draft(wid, None, "archived".into()).unwrap();
        mine_archived.assignees = vec!["benedikt".into()];
        mine_archived.archive().unwrap();

        for t in [
            &blocker,
            &mine_open,
            &mine_blocked,
            &someone_elses,
            &mine_archived,
        ] {
            ts.save(t, SnapshotSource::LocalEdit).await.unwrap();
        }

        let rows = svc.assigned_to(&wid.to_string(), "benedikt").await.unwrap();
        let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, vec!["open", "blocked"]);
        assert!(!rows[0].blocked);
        assert!(rows[1].blocked);
    }

    #[tokio::test]
    async fn drift_view_returns_only_divergent_sync_states() {
        let (svc, ws, _bs, ts) = svc();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        let draft = Task::new_draft(wid, None, "still drafting".into()).unwrap();
        let mut dirty = Task::new_draft(wid, None, "edited locally".into()).unwrap();
        dirty.stage_for_sync().unwrap();
        dirty
            .promote_to_remote(domain_task::RemoteRef::new("github", "42"))
            .unwrap();
        // promote_to_remote lands on Synced; flip to DirtyLocal to exercise drift.
        dirty.mark_dirty_local().unwrap();
        ts.save(&draft, SnapshotSource::LocalEdit).await.unwrap();
        ts.save(&dirty, SnapshotSource::LocalEdit).await.unwrap();

        let rows = svc.drift(&wid.to_string()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sync_state, "dirty_local");
        assert_eq!(rows[0].remote_id.as_deref(), Some("42"));
        // Projectless workspace → the project-status axis is inert: only the
        // sync reason is present and both project fields stay None.
        assert_eq!(rows[0].reasons, vec!["sync".to_string()]);
        assert_eq!(rows[0].project_status, None);
        assert_eq!(rows[0].project_status_expected, None);
    }

    /// Build a project attached to `ws` with a Backlog/In progress/Done option
    /// set and the standard Open→Backlog, InProgress→In progress, Done→Done
    /// mapping (Blocked intentionally unmapped, so it falls back to Open per
    /// §3). Saves it and wires `workspace.project_id`.
    async fn attach_project(
        ws: &mut Workspace,
        ws_repo: &Arc<InMemoryWorkspaceRepository>,
        projects: &Arc<InMemoryProjectRepository>,
    ) -> domain_project::Project {
        use domain_core::ProjectId;
        use domain_project::{Project, StatusMapping, StatusOption};
        let pid = ProjectId::parse("PVT_drift_test").unwrap();
        let opt = |id: &str, name: &str, ord: u32| StatusOption {
            option_id: id.into(),
            name: name.into(),
            ordinal: ord,
        };
        let project = Project::new(
            pid.clone(),
            "acme".into(),
            1,
            "Board".into(),
            "PVTSSF_field".into(),
            vec![
                opt("o_backlog", "Backlog", 0),
                opt("o_wip", "In progress", 1),
                opt("o_done", "Done", 2),
            ],
            vec![
                StatusMapping {
                    status: TaskStatus::Open,
                    option_id: "o_backlog".into(),
                },
                StatusMapping {
                    status: TaskStatus::InProgress,
                    option_id: "o_wip".into(),
                },
                StatusMapping {
                    status: TaskStatus::Done,
                    option_id: "o_done".into(),
                },
            ],
            false,
            domain_core::Timestamp::now(),
        )
        .unwrap();
        projects.save(&project).await.unwrap();
        ws.project_id = Some(pid);
        ws_repo.save(ws).await.unwrap();
        project
    }

    /// #39 case (a): a SYNCED task whose cached board status maps to a
    /// different option than its local status surfaces as project-status
    /// drift, even though `sync_state == synced`. Local says Open (→ Backlog);
    /// the board moved the card to Done.
    #[tokio::test]
    async fn drift_surfaces_synced_task_with_board_moved_to_done() {
        let (svc, ws, _bs, ts, ps) = svc_with_projects();
        let mut workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();
        attach_project(&mut workspace, &ws, &ps).await;

        // A fully-Synced, Open task whose board card was polled as "Done".
        let mut t = Task::new_draft(wid, None, "card moved on the board".into()).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", "7")).unwrap();
        assert_eq!(t.sync, SyncState::Synced);
        t.set_project_status_option_id(Some("o_done".into()));
        ts.save(&t, SnapshotSource::Push).await.unwrap();

        let rows = svc.drift(&wid.to_string()).await.unwrap();
        assert_eq!(rows.len(), 1, "the synced-but-board-moved task surfaces");
        let row = &rows[0];
        assert_eq!(row.sync_state, "synced", "sync axis is clean");
        assert_eq!(row.reasons, vec!["project_status".to_string()]);
        assert_eq!(row.project_status.as_deref(), Some("Done"));
        assert_eq!(row.project_status_expected.as_deref(), Some("Backlog"));
    }

    /// #39 case (b), reverse direction: REST/local says Done (→ Done option)
    /// but the board still shows "In progress".
    #[tokio::test]
    async fn drift_surfaces_board_behind_local_done() {
        let (svc, ws, _bs, ts, ps) = svc_with_projects();
        let mut workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();
        attach_project(&mut workspace, &ws, &ps).await;

        let mut t = Task::new_draft(wid, None, "local done, board lagging".into()).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", "8")).unwrap();
        t.start().unwrap();
        t.complete().unwrap();
        t.confirm_synced(SnapshotSource::Push).unwrap();
        assert_eq!(t.sync, SyncState::Synced);
        assert_eq!(t.status, TaskStatus::Done);
        t.set_project_status_option_id(Some("o_wip".into()));
        ts.save(&t, SnapshotSource::Push).await.unwrap();

        let rows = svc.drift(&wid.to_string()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].project_status.as_deref(), Some("In progress"));
        assert_eq!(rows[0].project_status_expected.as_deref(), Some("Done"));
        assert_eq!(rows[0].reasons, vec!["project_status".to_string()]);
    }

    /// #39 case (c): cached == expected → no project drift row.
    #[tokio::test]
    async fn drift_no_row_when_board_matches_local() {
        let (svc, ws, _bs, ts, ps) = svc_with_projects();
        let mut workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();
        attach_project(&mut workspace, &ws, &ps).await;

        // Open task, board cached as Backlog (= Open's mapped option). Agreement.
        let mut t = Task::new_draft(wid, None, "in agreement".into()).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", "9")).unwrap();
        t.set_project_status_option_id(Some("o_backlog".into()));
        ts.save(&t, SnapshotSource::Push).await.unwrap();

        let rows = svc.drift(&wid.to_string()).await.unwrap();
        assert!(rows.is_empty(), "no drift when board agrees with local");
    }

    /// #39 case (d): a projectless task → project_status None, not flagged.
    #[tokio::test]
    async fn drift_projectless_task_has_no_project_axis() {
        let (svc, ws, _bs, ts, _ps) = svc_with_projects();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        // A Synced task with a cached option id but NO project on the workspace.
        let mut t = Task::new_draft(wid, None, "no project".into()).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", "10")).unwrap();
        t.set_project_status_option_id(Some("o_done".into()));
        ts.save(&t, SnapshotSource::Push).await.unwrap();

        let rows = svc.drift(&wid.to_string()).await.unwrap();
        assert!(rows.is_empty(), "projectless workspace → no project drift");
    }

    /// #39 case (e): NULL cache (unpolled) → not flagged.
    #[tokio::test]
    async fn drift_null_cache_is_not_a_mismatch() {
        let (svc, ws, _bs, ts, ps) = svc_with_projects();
        let mut workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();
        attach_project(&mut workspace, &ws, &ps).await;

        // Open + Synced, project attached, but never polled (cache is None).
        let mut t = Task::new_draft(wid, None, "unpolled".into()).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", "11")).unwrap();
        assert_eq!(t.project_status_option_id, None);
        ts.save(&t, SnapshotSource::Push).await.unwrap();

        let rows = svc.drift(&wid.to_string()).await.unwrap();
        assert!(rows.is_empty(), "a NULL cache is unpolled, not a mismatch");
    }

    /// #39 case (f): a Blocked task with no Blocked option resolves to Open's
    /// option via the §3 fallback — so a board cached at that same Open option
    /// must NOT report phantom drift.
    #[tokio::test]
    async fn drift_blocked_with_no_blocked_option_no_phantom_drift() {
        let (svc, ws, _bs, ts, ps) = svc_with_projects();
        let mut workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();
        // The board has NO Blocked option (only Backlog/In progress/Done), so
        // Blocked falls back to Open → "Backlog".
        attach_project(&mut workspace, &ws, &ps).await;

        let mut t = Task::new_draft(wid, None, "blocked, board at backlog".into()).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", "12")).unwrap();
        t.mark_blocked().unwrap();
        t.confirm_synced(SnapshotSource::Push).unwrap();
        // Board card is at the Open/Backlog option — matches the fallback.
        t.set_project_status_option_id(Some("o_backlog".into()));
        ts.save(&t, SnapshotSource::Push).await.unwrap();

        let rows = svc.drift(&wid.to_string()).await.unwrap();
        assert!(
            rows.is_empty(),
            "Blocked→Open fallback must agree with a board cached at the Open option (no phantom drift)"
        );
    }

    /// A stale cached option id (the board option was renamed/removed remotely,
    /// so the project no longer owns it) must STILL flag drift — `is_drift` is
    /// computed from the raw option *ids* (`o_ghost != o_backlog`), not the
    /// resolved names. The actual status renders as `None` (no name to resolve)
    /// while the expected name resolves normally. This guards that a renamed
    /// remote option doesn't silently suppress the project axis (#39).
    #[tokio::test]
    async fn drift_stale_cached_option_id_still_flags_with_no_name() {
        let (svc, ws, _bs, ts, ps) = svc_with_projects();
        let mut workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();
        attach_project(&mut workspace, &ws, &ps).await;

        // Open task (expected → "Backlog"), but the board cached an option id
        // the project no longer owns (renamed/removed remotely).
        let mut t = Task::new_draft(wid, None, "stale cached option".into()).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", "14")).unwrap();
        t.set_project_status_option_id(Some("o_ghost".into()));
        ts.save(&t, SnapshotSource::Push).await.unwrap();

        let rows = svc.drift(&wid.to_string()).await.unwrap();
        assert_eq!(rows.len(), 1, "a stale cached id still flags drift");
        let row = &rows[0];
        assert_eq!(row.reasons, vec!["project_status".to_string()]);
        // No name for the unknown id; expected resolves normally.
        assert_eq!(row.project_status, None);
        assert_eq!(row.project_status_expected.as_deref(), Some("Backlog"));
    }

    /// #39 case: an `Archived` task with a cached project option is never
    /// flagged by the project axis. Two layers protect this and this test
    /// pins BOTH:
    ///
    /// 1. **Integration layer** — `drift()` lists with `include_archived:
    ///    false`, so an archived task never even reaches the axis (asserted via
    ///    `svc.drift` returning empty).
    /// 2. **Axis layer** — even if an archived task *did* reach it,
    ///    `project_axis` returns `is_drift == false` because
    ///    `resolved_option_id_for(Archived)` is `None` (no expected option).
    ///    Asserted by calling `project_axis` directly with a cached option id,
    ///    which the list filter would otherwise hide.
    #[tokio::test]
    async fn drift_archived_task_not_flagged() {
        let (svc, ws, _bs, ts, ps) = svc_with_projects();
        let mut workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();
        let project = attach_project(&mut workspace, &ws, &ps).await;

        let mut t = Task::new_draft(wid, None, "archived with cached option".into()).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", "15")).unwrap();
        t.set_project_status_option_id(Some("o_done".into()));
        t.archive().unwrap();
        ts.save(&t, SnapshotSource::Push).await.unwrap();

        // Layer 1: the archived task is filtered out of the drift list entirely.
        let rows = svc.drift(&wid.to_string()).await.unwrap();
        assert!(
            rows.is_empty(),
            "an Archived task is excluded from the drift list"
        );

        // Layer 2: even reaching the axis directly, Archived has no expected
        // option (`resolved_option_id_for(Archived) == None`) → no drift.
        let (actual, expected, is_drift) = project_axis(Some(&project), &t);
        assert!(
            !is_drift,
            "Archived → no expected option → never a mismatch"
        );
        assert_eq!(expected, None, "Archived maps to no project option");
        assert_eq!(
            actual.as_deref(),
            Some("Done"),
            "the cached actual name still resolves; only the expected side is None"
        );
    }

    /// A task that is BOTH sync-dirty AND project-status-drifted reports both
    /// reasons.
    #[tokio::test]
    async fn drift_reports_both_axes() {
        let (svc, ws, _bs, ts, ps) = svc_with_projects();
        let mut workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();
        attach_project(&mut workspace, &ws, &ps).await;

        let mut t = Task::new_draft(wid, None, "both axes".into()).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", "13")).unwrap();
        t.mark_dirty_local().unwrap(); // sync axis dirty
        t.set_project_status_option_id(Some("o_done".into())); // board moved
        ts.save(&t, SnapshotSource::Push).await.unwrap();

        let rows = svc.drift(&wid.to_string()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].reasons,
            vec!["sync".to_string(), "project_status".to_string()]
        );
        assert_eq!(rows[0].project_status.as_deref(), Some("Done"));
        assert_eq!(rows[0].project_status_expected.as_deref(), Some("Backlog"));
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

    /// rpl-sv2: a `Synced` task whose recorded `filing_repo_id`
    /// references a binding that's been deleted (e.g. an org-move
    /// replaced the binding, but the recorded UUID was never
    /// re-pointed) is the load-bearing case for the new
    /// `filing_repo` axis. Before this axis existed, such a task was
    /// invisible to `query drift` even though `rl task show` and
    /// `rl sync pull --task` would hard-fail. The new axis is the
    /// tripwire that closes the silent-divergence gap.
    #[tokio::test]
    async fn drift_surfaces_dangling_filing_repo_id_even_when_synced() {
        let (svc, ws, bs, ts, _ps) = svc_with_projects();
        let workspace = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
        let wid = workspace.id;
        ws.save(&workspace).await.unwrap();

        // The recorded filing binding. Will be deleted below to
        // simulate the silent-divergence shape.
        let old = RepoBinding::new(
            wid,
            "git@github.com:o/r-oldorg.git".into(),
            "github.com/o/r-oldorg".into(),
        )
        .unwrap();
        let old_id = old.id;
        bs.save(&old).await.unwrap();

        // A `Synced` task (via promote) with `filing_repo_id`
        // pointing at the (about-to-be-deleted) old binding. Logical
        // is *not* set (matches the rpl-sv2 repro where the live
        // bindings in the workspace's `repos` table no longer include
        // `old_id`).
        let mut t = Task::new_draft(wid, None, "dangling".into()).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", "1")).unwrap();
        t.force_set_filing_repo_id(Some(old_id));
        ts.save(&t, SnapshotSource::Push).await.unwrap();
        // Delete the binding — the silent divergence is now real.
        bs.delete(old_id).await.unwrap();

        let rows = svc.drift(&wid.to_string()).await.unwrap();
        assert_eq!(rows.len(), 1, "exactly the dangling task must surface");
        let row = &rows[0];
        assert_eq!(row.task_id, t.id.to_string());
        assert_eq!(row.sync_state, "synced");
        // The new axis must be the *only* reason — this is the
        // load-bearing assertion: a `Synced` task with a dangling
        // filing binding is invisible to drift WITHOUT this axis.
        assert_eq!(row.reasons, vec!["filing_repo".to_string()]);
    }
}
