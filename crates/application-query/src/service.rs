use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use domain_core::{TaskId, WorkspaceId};
use domain_repo::LinkStatus;
use domain_task::{RelationKind, SyncState, TaskStatus};
use ports::{RepoBindingRepository, TaskFilter, TaskRepository, WorkspaceRepository};

use crate::dto::{
    AssignedTaskRow, BlockedTaskRow, ChildTaskRow, ChildrenRollup, ContributorRow, DriftRow,
    ReadyTaskRow, StaleWorktreeRow, UnsyncedTaskRow, WorkspaceOverview,
};
use crate::error::Result;

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
    /// carries a `child_of` edge back to the parent. The union is loaded by id
    /// (a child may live in another workspace if related cross-repo).
    pub async fn children(&self, parent_id: &str) -> Result<ChildrenRollup> {
        let parent_uuid: TaskId = parent_id.parse()?;
        let parent = self.tasks.get(parent_uuid).await?;

        let mut child_ids: HashSet<TaskId> = parent
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::ParentOf)
            .map(|r| r.other)
            .collect();

        let workspace_tasks = self
            .tasks
            .list(TaskFilter {
                workspace_id: Some(parent.workspace_id),
                include_archived: true,
                ..TaskFilter::default()
            })
            .await?;
        for t in &workspace_tasks {
            if t.relations
                .iter()
                .any(|r| r.kind == RelationKind::ChildOf && r.other == parent.id)
            {
                child_ids.insert(t.id);
            }
        }

        let mut children = Vec::with_capacity(child_ids.len());
        for id in child_ids {
            let c = self.tasks.get(id).await?;
            children.push(ChildTaskRow {
                task_id: c.id.to_string(),
                title: c.title.clone(),
                status: enum_str(&c.status),
            });
        }
        // Incomplete first (done/archived sink to the bottom), then by title —
        // a stable order that puts outstanding work at the top.
        children.sort_by(|a, b| {
            let done = |s: &str| s == "done" || s == "archived";
            done(&a.status)
                .cmp(&done(&b.status))
                .then_with(|| a.title.cmp(&b.title))
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

    /// Tasks whose local sync state has diverged from the remote.
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
                    t.sync,
                    SyncState::DirtyLocal | SyncState::DirtyRemote | SyncState::Conflict
                )
            })
            .map(|t| DriftRow {
                task_id: t.id.to_string(),
                title: t.title.clone(),
                sync_state: enum_str(&t.sync),
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
