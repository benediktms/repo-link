//! [`WorkspaceService`] — workspace lifecycle + project-attach orchestration.

use std::sync::Arc;

use application_sync::enqueue;
use domain_core::{ProjectId, WorkspaceId};
use domain_sync::OutboxMutation;
use domain_task::SnapshotSource;
use domain_workspace::{Workspace, WorkspaceName};
use dto_shared::{CreateWorkspaceCmd, ListWorkspacesQuery, UpdateWorkspaceCmd, WorkspaceDto};
use ports::{
    OutboxRepository, PortError, ProjectRepository, TaskFilter, TaskRepository, WorkspaceRepository,
};

use crate::error::{Result, ServiceError};
use crate::mapping::workspace_to_dto;

pub struct WorkspaceService {
    repo: Arc<dyn WorkspaceRepository>,
    /// Optional `ProjectRepository` for the project-aware methods (`create`
    /// with `project_spec`, `set_project`). Callers that never need them —
    /// the daemon's internal services, most tests — wire only the workspace
    /// repo via `new`; the CLI wires both via `with_projects`.
    projects: Option<Arc<dyn ProjectRepository>>,
    /// Optional outbox + task repo for the eager set-project backfill (RFC
    /// 0001 §D1 / §7). Kept optional — like `projects` — so the daemon's plain
    /// `WorkspaceService::new` keeps working. When present, attaching a project
    /// enqueues `AddItem` for every issue-backed task not yet on the board.
    outbox: Option<Arc<dyn OutboxRepository>>,
    tasks: Option<Arc<dyn TaskRepository>>,
}

impl WorkspaceService {
    pub fn new(repo: Arc<dyn WorkspaceRepository>) -> Self {
        Self {
            repo,
            projects: None,
            outbox: None,
            tasks: None,
        }
    }

    pub fn with_projects(
        repo: Arc<dyn WorkspaceRepository>,
        projects: Arc<dyn ProjectRepository>,
    ) -> Self {
        Self {
            repo,
            projects: Some(projects),
            outbox: None,
            tasks: None,
        }
    }

    /// Wire the eager set-project backfill: when `set_project` attaches a
    /// project, every issue-backed task in the workspace that isn't already a
    /// board item gets an `AddItem` enqueued (the drainer's `AddItem`
    /// write-back then enqueues `SetProjectStatus`). The CLI composition root
    /// passes the outbox + task repo here; the daemon does not.
    pub fn with_outbox(
        mut self,
        outbox: Arc<dyn OutboxRepository>,
        tasks: Arc<dyn TaskRepository>,
    ) -> Self {
        self.outbox = Some(outbox);
        self.tasks = Some(tasks);
        self
    }

    pub async fn create(&self, cmd: CreateWorkspaceCmd) -> Result<WorkspaceDto> {
        let name = WorkspaceName::new(&cmd.name)?;
        if self.repo.find_by_name(name.as_str()).await?.is_some() {
            return Err(ServiceError::DuplicateName(name.as_str().to_string()));
        }
        let mut w = Workspace::new(name, cmd.description, cmd.local_only);
        if let Some(spec) = cmd.project_spec.as_deref() {
            w.project_id = Some(self.resolve_project(spec).await?);
        }
        self.repo.save(&w).await?;
        Ok(workspace_to_dto(&w))
    }

    pub async fn edit(&self, cmd: UpdateWorkspaceCmd) -> Result<WorkspaceDto> {
        if cmd.name.is_none() && cmd.description.is_none() {
            return Err(ServiceError::EmptyWorkspaceEdit);
        }

        let mut w = self.resolve_workspace(&cmd.workspace_id).await?;
        let name = match cmd.name {
            Some(raw) => {
                let name = WorkspaceName::new(raw)?;
                if name.as_str() != w.name.as_str()
                    && let Some(existing) = self.repo.find_by_name(name.as_str()).await?
                    && existing.id != w.id
                {
                    return Err(ServiceError::DuplicateName(name.as_str().to_string()));
                }
                Some(name)
            }
            None => None,
        };

        let duplicate_name = name.as_ref().map(|n| n.as_str().to_string());
        w.edit(name, cmd.description);
        match self.repo.save(&w).await {
            Ok(()) => {}
            Err(e) if e.conflict_target() == Some("workspaces.name") => {
                return Err(ServiceError::DuplicateName(
                    duplicate_name.unwrap_or_else(|| w.name.as_str().to_string()),
                ));
            }
            Err(e) => return Err(e.into()),
        }
        Ok(workspace_to_dto(&w))
    }

    /// Attach (`Some`) or detach (`None`) a workspace from a project.
    /// Resolution accepts a `PVT_…` node id or `owner/number`.
    ///
    /// **Eager backfill (RFC 0001 §D1 / §7).** When *attaching* a project, any
    /// *active* issue-backed task already in this workspace (`remote_id IS NOT
    /// NULL`) that isn't yet a board item (`project_item_id IS NULL`) gets an
    /// `AddItem` enqueued. Resolution is two-phase: `AddItem` now, then the
    /// drainer's `AddItem` write-back enqueues the `SetProjectStatus` once the
    /// returned `PVTI_…` is known. Tasks already on the board are skipped, and
    /// `set_project(None)` enqueues nothing. Backfill is a no-op unless the
    /// service was built with [`with_outbox`](Self::with_outbox).
    ///
    /// Scope: closed tasks (`Completed` / `NotPlanned`) are NOT back-filled —
    /// attaching a project is "put my *active* work on the board", not "drag
    /// my entire closed history onto a fresh board". Their AddItem would also
    /// produce no useful SetProjectStatus (closed tasks map to no option on a
    /// default board). Re-running attach (idempotent retry, or before the
    /// daemon drains) is deduped: a task that already has a pending `AddItem`
    /// is skipped, so a double-attach can't enqueue a second AddItem +
    /// follow-up SetProjectStatus per task.
    pub async fn set_project(
        &self,
        workspace_id: &str,
        project_spec: Option<&str>,
    ) -> Result<WorkspaceDto> {
        let id: WorkspaceId = workspace_id.parse()?;
        let mut w = self.repo.get(id).await?;
        let resolved = match project_spec {
            Some(spec) => Some(self.resolve_project(spec).await?),
            None => None,
        };

        // Reject a `Some(old) -> Some(new)` reassignment. The backfill below
        // assumes first-time attach: it skips tasks that already carry a
        // `project_item_id` (stale ids from the OLD board) and its AddItem
        // dedupe ignores which project an entry targets, so moving A -> B
        // would leave the old board's item ids attached under the new board.
        // First-time attach (`None -> Some`) and detach (`Some -> None`) are
        // still allowed; a no-op re-attach to the SAME project is fine.
        if let (Some(current), Some(requested)) = (&w.project_id, &resolved)
            && current != requested
        {
            return Err(ServiceError::ProjectReassignmentUnsupported {
                current: current.as_str().to_string(),
                requested: requested.as_str().to_string(),
            });
        }

        // Detach scrub (`-> None`): clear every task's `project_item_id` AND
        // cancel any still-pending `AddItem` for those tasks so a later
        // re-attach is a clean first-time attach. Without clearing the ids, the
        // stale ones (pointing at the OLD board) survive and the backfill below
        // skips those tasks as "already attached" — a backdoor around the
        // reassignment guard that would leave them anchored to a defunct board.
        // Cancelling pending AddItems matters too: a stale board add left in the
        // outbox would otherwise drain *after* detach and re-anchor the task to
        // the board it just left. This aligns with the §10.5 auto-detach
        // semantics: a detached task loses its local board anchor; the remote
        // board item is intentionally left untouched (full remote board cleanup
        // is a separate concern). The re-attach then backfills via the
        // idempotent `AddItem`. No-op unless the task repo is wired.
        //
        // Ordering + retryability (#54): the scrub runs BEFORE flipping
        // `project_id` to None, and it runs whenever the request is a detach
        // (`resolved.is_none()`), NOT only when the workspace is currently
        // attached. If a `tasks.save` fails partway, the workspace is still
        // attached (the flip hasn't happened), so a retry sees the same detach
        // request and re-scrubs the residual ids — `set_project(None)` always
        // completes the scrub even when the workspace is already detached. The
        // per-task work is idempotent (clear only when set; delete is a no-op
        // when nothing's pending), so a retry never double-acts.
        if resolved.is_none()
            && let Some(tasks) = &self.tasks
        {
            let workspace_tasks = tasks
                .list(TaskFilter {
                    workspace_id: Some(id),
                    // `is_open: None` = both open and closed: the scrub must
                    // clear residual project ids off every task regardless of
                    // lifecycle.
                    ..TaskFilter::default()
                })
                .await?;
            for mut task in workspace_tasks {
                if let Some(outbox) = &self.outbox {
                    outbox.delete_pending_add_items(task.id).await?;
                }
                if task.project_item_id.is_some() {
                    task.project_item_id = None;
                    tasks.save(&task, SnapshotSource::LocalEdit).await?;
                }
            }
        }

        w.project_id = resolved.clone();
        self.repo.save(&w).await?;

        // Backfill only on attach, and only when both the project repo and the
        // outbox/task handles are wired.
        if let (Some(project_id), Some(outbox), Some(tasks), Some(projects)) =
            (resolved, &self.outbox, &self.tasks, &self.projects)
        {
            let project = projects.get(project_id).await?;
            // `is_open: Some(true)` keeps only open tasks: attach back-fills
            // *active* work, not closed history (both closed lifecycle
            // variants — Completed and NotPlanned — are excluded by the filter).
            let workspace_tasks = tasks
                .list(TaskFilter {
                    workspace_id: Some(id),
                    is_open: Some(true),
                    ..TaskFilter::default()
                })
                .await?;
            for task in workspace_tasks {
                // Issue-backed (has a GraphQL node id to attach) AND not yet a
                // board item. Drafts have no issue node id, so they can't be
                // `AddItem`'d — they're created directly via CreateDraftIssue
                // on their own promote path.
                if task.project_item_id.is_some() {
                    continue;
                }
                // No remote at all → a local-only / draft task. Skip silently:
                // it isn't board-eligible via AddItem (it goes through
                // CreateDraftIssue on promote), and "run `rl sync pull`" would
                // be misleading advice for a task with nothing to pull.
                let Some(remote) = task.remote.as_ref() else {
                    continue;
                };
                let Some(node_id) = remote.node_id.clone() else {
                    // Issue-backed but no GraphQL node id → can't AddItem.
                    // Pre-project-sync tasks recorded a remote before node ids
                    // were persisted; they backfill it on their next `sync
                    // pull`. Log instead of skipping silently so a "0 added"
                    // backfill is diagnosable (RFC 0001 §9 / §D1).
                    tracing::warn!(
                        task_id = %task.id,
                        remote_id = remote.remote_id.as_str(),
                        "set-project backfill: task has no remote node_id; skipping AddItem (run `rl sync pull` on it to backfill the node id)"
                    );
                    continue;
                };
                // Dedup against a re-run / pre-drain re-attach: if this task
                // already has a pending AddItem **for THIS project**, don't
                // enqueue a second one (which would also trigger a duplicate
                // SetProjectStatus follow-up via the drainer's write-back).
                // Mirrors the daemon's startup-reconcile guard.
                // addProjectV2ItemById is idempotent remotely, but the outbox
                // shouldn't accumulate redundant work. The `project_node_id`
                // match is load-bearing: a stale AddItem left pending for the
                // OLD project must NOT suppress this attach to a NEW project
                // (#54) — they target different boards.
                let already_queued = outbox.list_pending(task.id).await?.iter().any(|e| {
                    matches!(
                        &e.mutation,
                        OutboxMutation::AddItem { project_node_id, .. }
                            if project_node_id == project.id.as_str()
                    )
                });
                if already_queued {
                    continue;
                }
                enqueue::enqueue(
                    outbox,
                    task.id,
                    OutboxMutation::AddItem {
                        project_node_id: project.id.as_str().to_string(),
                        issue_node_id: node_id,
                    },
                )
                .await?;
            }
        }

        Ok(workspace_to_dto(&w))
    }

    /// Attach (`Some`) or clear (`None`) the workspace's default filing repo.
    ///
    /// `repo_id` is the **pre-resolved binding UUID string** (resolved in the
    /// CLI dispatch layer via `resolve_repo_handle_required`, exactly as
    /// `task create` / `task edit` resolve `--repo`). This keeps
    /// `WorkspaceService` free of a `RepoBindingRepository` dependency and
    /// inherits the ambiguous-candidates + exit-2 UX for free.
    ///
    /// **Deliberate divergences from `set_project`:**
    ///
    /// * Reassignment of an already-set default **is allowed** — the default is
    ///   forward-looking: only tasks resolved *after* the change are affected.
    ///   Already-recorded `tasks.filing_repo_id` values are authoritative (D2
    ///   never re-resolves), so there is no stale-id problem and no need for a
    ///   reassignment guard.
    /// * **No eager backfill** — filing is recorded on promote, not eagerly like
    ///   `set_project`'s board backfill. So this method is a plain
    ///   load-flip-save and works on a bare `WorkspaceService::new` (no
    ///   outbox / tasks / projects handles required).
    pub async fn set_filing_repo(
        &self,
        workspace_id: &str,
        repo_id: Option<&str>,
    ) -> Result<WorkspaceDto> {
        let id: WorkspaceId = workspace_id.parse()?;
        let mut w = self.repo.get(id).await?;
        let parsed = repo_id.map(str::parse).transpose()?;
        // Domain setter bumps updated_at so the mutation is observable.
        w.set_filing_repo_id(parsed);
        self.repo.save(&w).await?;
        Ok(workspace_to_dto(&w))
    }

    /// Resolve a `<project-spec>` to a `ProjectId`. Centralised here so the
    /// CLI and service share one form. `owner/number` falls through to a
    /// `list_all` scan because projects have no `UNIQUE(owner, number)` —
    /// they're addressed by node id everywhere downstream.
    async fn resolve_project(&self, spec: &str) -> Result<ProjectId> {
        let projects = self
            .projects
            .as_ref()
            .ok_or(ServiceError::ProjectsUnconfigured)?;
        let trimmed = spec.trim();
        if let Ok(id) = ProjectId::parse(trimmed.to_string()) {
            // Confirm the id actually corresponds to a known project so we
            // don't store a dangling FK reference. Normalize NotFound here
            // so callers see one shape regardless of node-id vs owner/number.
            projects.get(id.clone()).await.map_err(|e| match e {
                PortError::NotFound(_) => ServiceError::ProjectNotFound(spec.to_string()),
                other => ServiceError::Port(other),
            })?;
            return Ok(id);
        }
        let (owner, number_str) = trimmed
            .split_once('/')
            .ok_or_else(|| ServiceError::ProjectNotFound(spec.to_string()))?;
        let number: u64 = number_str
            .parse()
            .map_err(|_| ServiceError::ProjectNotFound(spec.to_string()))?;
        let all = projects.list_all().await?;
        all.into_iter()
            .find(|p| p.owner_login == owner && p.number == number)
            .map(|p| p.id)
            .ok_or_else(|| ServiceError::ProjectNotFound(spec.to_string()))
    }

    pub async fn show(&self, id: &str) -> Result<WorkspaceDto> {
        let id: WorkspaceId = id.parse()?;
        let w = self.repo.get(id).await?;
        Ok(workspace_to_dto(&w))
    }

    pub async fn list(&self, query: ListWorkspacesQuery) -> Result<Vec<WorkspaceDto>> {
        let rows = self.repo.list(query.include_archived).await?;
        Ok(rows.iter().map(workspace_to_dto).collect())
    }

    pub async fn activate(&self, id: &str) -> Result<WorkspaceDto> {
        self.transition(id, |w| w.activate()).await
    }

    pub async fn pause(&self, id: &str) -> Result<WorkspaceDto> {
        self.transition(id, |w| w.pause()).await
    }

    pub async fn archive(&self, id: &str) -> Result<WorkspaceDto> {
        self.transition(id, |w| w.archive()).await
    }

    pub async fn unarchive(&self, id: &str) -> Result<WorkspaceDto> {
        self.transition(id, |w| w.unarchive()).await
    }

    async fn transition<F>(&self, id: &str, op: F) -> Result<WorkspaceDto>
    where
        F: FnOnce(&mut Workspace) -> domain_core::Result<()>,
    {
        let id: WorkspaceId = id.parse()?;
        let mut w = self.repo.get(id).await?;
        op(&mut w)?;
        self.repo.save(&w).await?;
        Ok(workspace_to_dto(&w))
    }

    async fn resolve_workspace(&self, id_or_name: &str) -> Result<Workspace> {
        if let Ok(id) = id_or_name.parse::<WorkspaceId>() {
            match self.repo.get(id).await {
                Ok(w) => return Ok(w),
                Err(PortError::NotFound(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.repo
            .find_by_name(id_or_name)
            .await?
            .ok_or_else(|| ServiceError::WorkspaceNotFound(id_or_name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RepoBindingService;
    use ports::RepoBindingRepository;
    use testing_fixtures::{InMemoryRepoBindingRepository, InMemoryWorkspaceRepository};

    fn setup() -> (WorkspaceService, RepoBindingService) {
        let workspaces: Arc<dyn WorkspaceRepository> = Arc::new(InMemoryWorkspaceRepository::new());
        let bindings: Arc<dyn RepoBindingRepository> =
            Arc::new(InMemoryRepoBindingRepository::new());
        (
            WorkspaceService::new(workspaces.clone()),
            RepoBindingService::new(workspaces, bindings),
        )
    }

    #[tokio::test]
    async fn create_show_and_list_workspace() {
        let (svc, _) = setup();
        let dto = svc
            .create(CreateWorkspaceCmd {
                name: "scratch".into(),
                description: None,
                local_only: true,
                project_spec: None,
            })
            .await
            .unwrap();
        assert_eq!(dto.status, "created");
        assert_eq!(svc.show(&dto.id).await.unwrap(), dto);
        assert_eq!(
            svc.list(ListWorkspacesQuery::default())
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn duplicate_name_rejected() {
        let (svc, _) = setup();
        svc.create(CreateWorkspaceCmd {
            name: "a".into(),
            description: None,
            local_only: true,
            project_spec: None,
        })
        .await
        .unwrap();
        let err = svc
            .create(CreateWorkspaceCmd {
                name: "a".into(),
                description: None,
                local_only: true,
                project_spec: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::DuplicateName(_)));
    }

    #[tokio::test]
    async fn activate_and_archive_transition_dto_status() {
        let (svc, _) = setup();
        let dto = svc
            .create(CreateWorkspaceCmd {
                name: "demo".into(),
                description: None,
                local_only: false,
                project_spec: None,
            })
            .await
            .unwrap();
        let active = svc.activate(&dto.id).await.unwrap();
        assert_eq!(active.status, "active");
        let archived = svc.archive(&dto.id).await.unwrap();
        assert_eq!(archived.status, "archived");
        let revived = svc.unarchive(&dto.id).await.unwrap();
        assert_eq!(
            revived.status, "active",
            "unarchive returns Archived → Active"
        );
    }

    // ---------- Stage 6 (#54): eager set-project backfill ------------------

    use domain_project::{Project, StatusMapping, StatusOption};
    use domain_task::{RemoteRef, SnapshotSource, Task};
    use ports::OutboxRepository;
    use testing_fixtures::{
        InMemoryOutboxRepository, InMemoryProjectRepository, InMemoryTaskRepository,
    };

    fn backfill_project(id: &str) -> Project {
        Project::new(
            ProjectId::parse(id).unwrap(),
            "acme".into(),
            5,
            "Board".into(),
            "PVTSSF_field".into(),
            vec![StatusOption {
                option_id: "o1".into(),
                name: "Backlog".into(),
                ordinal: 0,
            }],
            vec![StatusMapping {
                is_open: true,
                option_id: "o1".into(),
            }],
            false,
            domain_core::Timestamp::now(),
        )
        .unwrap()
    }

    /// Save a synced issue-backed mirror with the given node id / item id.
    async fn save_mirror(
        tasks: &Arc<InMemoryTaskRepository>,
        ws: WorkspaceId,
        node_id: Option<&str>,
        project_item_id: Option<&str>,
    ) -> Task {
        let mut t = Task::new_draft(ws, None, "m".into()).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef {
            provider: "github".into(),
            remote_id: "1".into(),
            node_id: node_id.map(str::to_owned),
        })
        .unwrap();
        t.project_item_id = project_item_id.map(str::to_owned);
        tasks.save(&t, SnapshotSource::Promote).await.unwrap();
        t
    }

    #[tokio::test]
    async fn set_project_some_enqueues_add_item_for_each_unattached_issue() {
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let projects = Arc::new(InMemoryProjectRepository::new());
        let outbox = Arc::new(InMemoryOutboxRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());

        let project = backfill_project("PVT_kwHO_bf");
        projects.save(&project).await.unwrap();

        let ws_repo_dyn: Arc<dyn WorkspaceRepository> = ws_repo.clone();
        let proj_dyn: Arc<dyn ProjectRepository> = projects.clone();
        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let tasks_dyn: Arc<dyn TaskRepository> = tasks.clone();
        let svc = WorkspaceService::with_projects(ws_repo_dyn, proj_dyn)
            .with_outbox(outbox_dyn, tasks_dyn);

        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws_repo.save(&ws).await.unwrap();

        // Two issue-backed mirrors with node ids, project_item_id NULL.
        save_mirror(&tasks, ws.id, Some("I_a"), None).await;
        save_mirror(&tasks, ws.id, Some("I_b"), None).await;

        svc.set_project(&ws.id.to_string(), Some(project.id.as_str()))
            .await
            .unwrap();

        let all = outbox.all();
        assert_eq!(all.len(), 2, "one AddItem per unattached issue");
        assert!(all.iter().all(|e| e.mutation.kind() == "add_item"));
    }

    #[tokio::test]
    async fn set_project_skips_issue_with_no_node_id() {
        // rpl-4ui: a pre-project-sync task carries a remote_id but no GraphQL
        // node id, so it can't be AddItem'd. It's skipped (and logged — the
        // skip is no longer silent), not enqueued with a bogus node id.
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let projects = Arc::new(InMemoryProjectRepository::new());
        let outbox = Arc::new(InMemoryOutboxRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());

        let project = backfill_project("PVT_kwHO_nonode");
        projects.save(&project).await.unwrap();

        let ws_repo_dyn: Arc<dyn WorkspaceRepository> = ws_repo.clone();
        let proj_dyn: Arc<dyn ProjectRepository> = projects.clone();
        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let tasks_dyn: Arc<dyn TaskRepository> = tasks.clone();
        let svc = WorkspaceService::with_projects(ws_repo_dyn, proj_dyn)
            .with_outbox(outbox_dyn, tasks_dyn);

        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws_repo.save(&ws).await.unwrap();

        // One issue-backed mirror WITHOUT a node id, plus one with — only the
        // node-id-bearing task is enqueued.
        save_mirror(&tasks, ws.id, None, None).await;
        save_mirror(&tasks, ws.id, Some("I_has_node"), None).await;

        svc.set_project(&ws.id.to_string(), Some(project.id.as_str()))
            .await
            .unwrap();

        let all = outbox.all();
        assert_eq!(all.len(), 1, "only the node-id-bearing task is enqueued");
        assert!(matches!(
            &all[0].mutation,
            OutboxMutation::AddItem { issue_node_id, .. } if issue_node_id == "I_has_node"
        ));
    }

    #[tokio::test]
    async fn set_project_none_enqueues_nothing() {
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let projects = Arc::new(InMemoryProjectRepository::new());
        let outbox = Arc::new(InMemoryOutboxRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());

        let ws_repo_dyn: Arc<dyn WorkspaceRepository> = ws_repo.clone();
        let proj_dyn: Arc<dyn ProjectRepository> = projects.clone();
        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let tasks_dyn: Arc<dyn TaskRepository> = tasks.clone();
        let svc = WorkspaceService::with_projects(ws_repo_dyn, proj_dyn)
            .with_outbox(outbox_dyn, tasks_dyn);

        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws_repo.save(&ws).await.unwrap();
        save_mirror(&tasks, ws.id, Some("I_a"), None).await;

        svc.set_project(&ws.id.to_string(), None).await.unwrap();
        assert!(outbox.all().is_empty(), "detach enqueues nothing");
    }

    #[tokio::test]
    async fn detach_clears_project_item_id_so_reattach_is_a_clean_first_time_attach() {
        // Regression (#54): detach (`Some -> None`) must clear each task's
        // `project_item_id`. Otherwise a stale id (pointing at the OLD board)
        // survives, and a later attach to a DIFFERENT project skips the task as
        // "already attached" — leaving it anchored to a defunct board, a
        // backdoor around the reassignment guard. After clearing, re-attaching
        // to project B is a clean first-time attach that backfills via AddItem.
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let projects = Arc::new(InMemoryProjectRepository::new());
        let outbox = Arc::new(InMemoryOutboxRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());

        let project_a = backfill_project("PVT_kwHO_detach_a");
        let project_b = backfill_project("PVT_kwHO_detach_b");
        projects.save(&project_a).await.unwrap();
        projects.save(&project_b).await.unwrap();

        let ws_repo_dyn: Arc<dyn WorkspaceRepository> = ws_repo.clone();
        let proj_dyn: Arc<dyn ProjectRepository> = projects.clone();
        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let tasks_dyn: Arc<dyn TaskRepository> = tasks.clone();
        let svc = WorkspaceService::with_projects(ws_repo_dyn, proj_dyn)
            .with_outbox(outbox_dyn, tasks_dyn);

        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws_repo.save(&ws).await.unwrap();

        // A task already on board A (seeded with a project_item_id).
        let task = save_mirror(&tasks, ws.id, Some("I_a"), Some("PVTI_old_a")).await;

        // Attach to A — the task is already attached, so backfill skips it.
        svc.set_project(&ws.id.to_string(), Some(project_a.id.as_str()))
            .await
            .unwrap();

        // Detach — the stale project_item_id must be cleared.
        svc.set_project(&ws.id.to_string(), None).await.unwrap();
        let cleared = tasks.get(task.id).await.unwrap();
        assert_eq!(
            cleared.project_item_id, None,
            "detach clears the stale board item id"
        );

        // Re-attach to a DIFFERENT project (B). With the id cleared, the task
        // is a clean first-time attach and is backfilled, not skipped.
        svc.set_project(&ws.id.to_string(), Some(project_b.id.as_str()))
            .await
            .unwrap();

        let add_items: Vec<_> = outbox
            .all()
            .into_iter()
            .filter(|e| e.mutation.kind() == "add_item")
            .collect();
        assert_eq!(
            add_items.len(),
            1,
            "the re-attached task is backfilled for B, not skipped as already-attached"
        );
        if let OutboxMutation::AddItem {
            project_node_id, ..
        } = &add_items[0].mutation
        {
            assert_eq!(
                project_node_id,
                project_b.id.as_str(),
                "backfill targets the new board B"
            );
        } else {
            panic!("expected an AddItem mutation");
        }
    }

    #[tokio::test]
    async fn set_project_skips_already_attached_task() {
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let projects = Arc::new(InMemoryProjectRepository::new());
        let outbox = Arc::new(InMemoryOutboxRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());

        let project = backfill_project("PVT_kwHO_skip");
        projects.save(&project).await.unwrap();

        let ws_repo_dyn: Arc<dyn WorkspaceRepository> = ws_repo.clone();
        let proj_dyn: Arc<dyn ProjectRepository> = projects.clone();
        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let tasks_dyn: Arc<dyn TaskRepository> = tasks.clone();
        let svc = WorkspaceService::with_projects(ws_repo_dyn, proj_dyn)
            .with_outbox(outbox_dyn, tasks_dyn);

        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws_repo.save(&ws).await.unwrap();

        // One unattached + one already on the board.
        save_mirror(&tasks, ws.id, Some("I_a"), None).await;
        save_mirror(&tasks, ws.id, Some("I_b"), Some("PVTI_b")).await;

        svc.set_project(&ws.id.to_string(), Some(project.id.as_str()))
            .await
            .unwrap();

        let all = outbox.all();
        assert_eq!(all.len(), 1, "the already-attached task is skipped");
        assert_eq!(all[0].mutation.kind(), "add_item");
    }

    #[tokio::test]
    async fn set_project_twice_does_not_duplicate_add_item() {
        // A repeated / idempotent attach (or a re-run before the daemon drains
        // and writes project_item_id back) must NOT enqueue a second AddItem
        // per task — that would also fan out a duplicate SetProjectStatus.
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let projects = Arc::new(InMemoryProjectRepository::new());
        let outbox = Arc::new(InMemoryOutboxRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());

        let project = backfill_project("PVT_kwHO_dup");
        projects.save(&project).await.unwrap();

        let ws_repo_dyn: Arc<dyn WorkspaceRepository> = ws_repo.clone();
        let proj_dyn: Arc<dyn ProjectRepository> = projects.clone();
        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let tasks_dyn: Arc<dyn TaskRepository> = tasks.clone();
        let svc = WorkspaceService::with_projects(ws_repo_dyn, proj_dyn)
            .with_outbox(outbox_dyn, tasks_dyn);

        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws_repo.save(&ws).await.unwrap();
        save_mirror(&tasks, ws.id, Some("I_a"), None).await;

        // First attach enqueues one AddItem.
        svc.set_project(&ws.id.to_string(), Some(project.id.as_str()))
            .await
            .unwrap();
        // Second attach (project_item_id still NULL — daemon hasn't drained)
        // must be a no-op for the already-queued task.
        svc.set_project(&ws.id.to_string(), Some(project.id.as_str()))
            .await
            .unwrap();

        let add_items = outbox
            .all()
            .into_iter()
            .filter(|e| e.mutation.kind() == "add_item")
            .count();
        assert_eq!(add_items, 1, "exactly one AddItem after a double attach");
    }

    #[tokio::test]
    async fn set_project_rejects_reassignment_between_projects() {
        // Moving an already-attached workspace from project A to project B is
        // rejected: the backfill assumes first-time attach and would leave
        // stale A-board item ids under B. Detach-then-attach is the supported
        // path until a migration is designed.
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let projects = Arc::new(InMemoryProjectRepository::new());
        let outbox = Arc::new(InMemoryOutboxRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());

        let project_a = backfill_project("PVT_kwHO_a");
        let project_b = backfill_project("PVT_kwHO_b");
        projects.save(&project_a).await.unwrap();
        projects.save(&project_b).await.unwrap();

        let ws_repo_dyn: Arc<dyn WorkspaceRepository> = ws_repo.clone();
        let proj_dyn: Arc<dyn ProjectRepository> = projects.clone();
        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let tasks_dyn: Arc<dyn TaskRepository> = tasks.clone();
        let svc = WorkspaceService::with_projects(ws_repo_dyn, proj_dyn)
            .with_outbox(outbox_dyn, tasks_dyn);

        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws_repo.save(&ws).await.unwrap();

        // First-time attach to A succeeds.
        svc.set_project(&ws.id.to_string(), Some(project_a.id.as_str()))
            .await
            .unwrap();

        // Reassigning to B is rejected.
        let err = svc
            .set_project(&ws.id.to_string(), Some(project_b.id.as_str()))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ServiceError::ProjectReassignmentUnsupported { .. }
        ));

        // The stored project is unchanged (still A) after the rejected move.
        let reloaded = ws_repo.get(ws.id).await.unwrap();
        assert_eq!(reloaded.project_id.as_ref(), Some(&project_a.id));

        // A no-op re-attach to the SAME project is still allowed, and detach
        // (Some -> None) is allowed.
        svc.set_project(&ws.id.to_string(), Some(project_a.id.as_str()))
            .await
            .expect("re-attach to the same project is a no-op, not a reassignment");
        svc.set_project(&ws.id.to_string(), None)
            .await
            .expect("detach is allowed");
    }

    #[tokio::test]
    async fn set_project_skips_done_and_archived_tasks() {
        // Attach back-fills *active* work, not closed history: Done / Archived
        // issue-backed tasks are not added to the new board.
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let projects = Arc::new(InMemoryProjectRepository::new());
        let outbox = Arc::new(InMemoryOutboxRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());

        let project = backfill_project("PVT_kwHO_terminal");
        projects.save(&project).await.unwrap();

        let ws_repo_dyn: Arc<dyn WorkspaceRepository> = ws_repo.clone();
        let proj_dyn: Arc<dyn ProjectRepository> = projects.clone();
        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let tasks_dyn: Arc<dyn TaskRepository> = tasks.clone();
        let svc = WorkspaceService::with_projects(ws_repo_dyn, proj_dyn)
            .with_outbox(outbox_dyn, tasks_dyn);

        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws_repo.save(&ws).await.unwrap();

        // One active (Open) + one Done + one Archived, all issue-backed + unattached.
        save_mirror(&tasks, ws.id, Some("I_open"), None).await;

        let mut done = save_mirror(&tasks, ws.id, Some("I_done"), None).await;
        done.start().unwrap();
        done.complete().unwrap();
        tasks.save(&done, SnapshotSource::LocalEdit).await.unwrap();

        let mut archived = save_mirror(&tasks, ws.id, Some("I_arch"), None).await;
        archived.archive().unwrap();
        tasks
            .save(&archived, SnapshotSource::LocalEdit)
            .await
            .unwrap();

        svc.set_project(&ws.id.to_string(), Some(project.id.as_str()))
            .await
            .unwrap();

        let add_items: Vec<_> = outbox
            .all()
            .into_iter()
            .filter(|e| e.mutation.kind() == "add_item")
            .collect();
        assert_eq!(
            add_items.len(),
            1,
            "only the active (Open) task is back-filled; Done/Archived skipped"
        );
    }

    // ---------- set_filing_repo (RFC 0002 §4 / GitHub #121) ----------------

    #[tokio::test]
    async fn set_filing_repo_records_default_and_surfaces_on_dto() {
        // A bare WorkspaceService::new suffices — no projects/outbox wired.
        let workspaces: Arc<dyn WorkspaceRepository> = Arc::new(InMemoryWorkspaceRepository::new());
        let svc = WorkspaceService::new(workspaces.clone());

        let ws = Workspace::new(WorkspaceName::new("r").unwrap(), None, true);
        let ws_id = ws.id.to_string();
        workspaces.save(&ws).await.unwrap();

        // Use a valid UUID as the pre-resolved binding id.
        let repo_uuid = domain_core::RepoId::new().to_string();
        let dto = svc.set_filing_repo(&ws_id, Some(&repo_uuid)).await.unwrap();
        assert_eq!(
            dto.filing_repo_id.as_deref(),
            Some(repo_uuid.as_str()),
            "filing_repo_id must appear on the returned WorkspaceDto"
        );

        // Reload and verify persistence.
        let reloaded = svc.show(&ws_id).await.unwrap();
        assert_eq!(reloaded.filing_repo_id, dto.filing_repo_id);
    }

    #[tokio::test]
    async fn set_filing_repo_none_clears_the_default() {
        let workspaces: Arc<dyn WorkspaceRepository> = Arc::new(InMemoryWorkspaceRepository::new());
        let svc = WorkspaceService::new(workspaces.clone());

        let ws = Workspace::new(WorkspaceName::new("r2").unwrap(), None, true);
        let ws_id = ws.id.to_string();
        workspaces.save(&ws).await.unwrap();

        let repo_uuid = domain_core::RepoId::new().to_string();
        svc.set_filing_repo(&ws_id, Some(&repo_uuid)).await.unwrap();

        let cleared = svc.set_filing_repo(&ws_id, None).await.unwrap();
        assert!(
            cleared.filing_repo_id.is_none(),
            "passing None must clear the default"
        );
    }

    #[tokio::test]
    async fn set_filing_repo_reassignment_succeeds() {
        // Unlike set_project, reassigning Some(a) -> Some(b) is ALLOWED.
        let workspaces: Arc<dyn WorkspaceRepository> = Arc::new(InMemoryWorkspaceRepository::new());
        let svc = WorkspaceService::new(workspaces.clone());

        let ws = Workspace::new(WorkspaceName::new("r3").unwrap(), None, true);
        let ws_id = ws.id.to_string();
        workspaces.save(&ws).await.unwrap();

        let repo_a = domain_core::RepoId::new().to_string();
        let repo_b = domain_core::RepoId::new().to_string();

        svc.set_filing_repo(&ws_id, Some(&repo_a)).await.unwrap();
        let dto = svc
            .set_filing_repo(&ws_id, Some(&repo_b))
            .await
            .expect("reassignment of an already-set default must succeed");
        assert_eq!(
            dto.filing_repo_id.as_deref(),
            Some(repo_b.as_str()),
            "dto must reflect the new (B) binding after reassignment"
        );
    }

    #[tokio::test]
    async fn set_filing_repo_works_on_bare_service_without_projects_or_outbox() {
        // Filing needs no projects / outbox / tasks — WorkspaceService::new suffices.
        let workspaces: Arc<dyn WorkspaceRepository> = Arc::new(InMemoryWorkspaceRepository::new());
        let svc = WorkspaceService::new(workspaces.clone());

        let ws = Workspace::new(WorkspaceName::new("r4").unwrap(), None, true);
        let ws_id = ws.id.to_string();
        workspaces.save(&ws).await.unwrap();

        let repo_uuid = domain_core::RepoId::new().to_string();
        // Must not panic or return an error because projects/outbox are None.
        svc.set_filing_repo(&ws_id, Some(&repo_uuid))
            .await
            .expect("set_filing_repo must work on a bare WorkspaceService::new");
    }

    #[tokio::test]
    async fn pending_add_item_for_old_project_does_not_block_backfill_for_new_project() {
        // Regression (#54): the backfill dedupe must key on `project_node_id`.
        // A stale `AddItem` left pending for the OLD project (A) must NOT
        // suppress a fresh attach's backfill for a DIFFERENT project (B) — they
        // target different boards. Construct the leftover-pending state
        // directly (a partially-drained earlier attach), then attach to B and
        // assert B's backfill still fires.
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let projects = Arc::new(InMemoryProjectRepository::new());
        let outbox = Arc::new(InMemoryOutboxRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());

        let project_a = backfill_project("PVT_kwHO_pending_a");
        let project_b = backfill_project("PVT_kwHO_pending_b");
        projects.save(&project_a).await.unwrap();
        projects.save(&project_b).await.unwrap();

        let ws_repo_dyn: Arc<dyn WorkspaceRepository> = ws_repo.clone();
        let proj_dyn: Arc<dyn ProjectRepository> = projects.clone();
        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let tasks_dyn: Arc<dyn TaskRepository> = tasks.clone();
        let svc = WorkspaceService::with_projects(ws_repo_dyn, proj_dyn)
            .with_outbox(outbox_dyn, tasks_dyn);

        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws_repo.save(&ws).await.unwrap();

        // An unattached issue-backed task carrying a STALE pending AddItem for
        // project A (left over from an earlier, abandoned attach to A).
        let task = save_mirror(&tasks, ws.id, Some("I_x"), None).await;
        let stale = domain_sync::OutboxEntry::new(
            task.id,
            OutboxMutation::AddItem {
                project_node_id: project_a.id.as_str().to_string(),
                issue_node_id: "I_x".into(),
            },
        );
        outbox.enqueue(&stale).await.unwrap();

        // Attach to project B (the workspace is currently unattached, so this
        // is a clean first-time attach — no reassignment guard).
        svc.set_project(&ws.id.to_string(), Some(project_b.id.as_str()))
            .await
            .unwrap();

        // Both AddItems exist now: the stale A one AND the fresh B backfill.
        // The A entry must NOT have suppressed B.
        let add_b: Vec<_> = outbox
            .all()
            .into_iter()
            .filter_map(|e| match e.mutation {
                OutboxMutation::AddItem {
                    project_node_id, ..
                } if project_node_id == project_b.id.as_str() => Some(project_node_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            add_b.len(),
            1,
            "a pending AddItem for project A must not block backfill for project B"
        );
    }

    #[tokio::test]
    async fn detach_scrub_completes_when_workspace_already_detached() {
        // Regression (#54): the detach scrub must be retryable. The OLD code
        // gated the scrub on `w.project_id.is_some()` and flipped `project_id`
        // to None *before* clearing the per-task ids — so a `tasks.save` that
        // failed partway left the workspace detached but tasks still carrying
        // stale ids, and a retry was a no-op (the gate saw `project_id == None`,
        // so `detaching` was false). The fix runs the scrub on any detach
        // request (`resolved.is_none()`), even when the workspace is already
        // detached. Model the partial-failure aftermath directly: workspace
        // detached (project_id = None) but a task still carries a stale
        // project_item_id AND a pending AddItem. A `set_project(None)` retry
        // must finish the scrub.
        let ws_repo = Arc::new(InMemoryWorkspaceRepository::new());
        let projects = Arc::new(InMemoryProjectRepository::new());
        let outbox = Arc::new(InMemoryOutboxRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());

        let project = backfill_project("PVT_kwHO_retry");
        projects.save(&project).await.unwrap();

        let ws_repo_dyn: Arc<dyn WorkspaceRepository> = ws_repo.clone();
        let proj_dyn: Arc<dyn ProjectRepository> = projects.clone();
        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let tasks_dyn: Arc<dyn TaskRepository> = tasks.clone();
        let svc = WorkspaceService::with_projects(ws_repo_dyn, proj_dyn)
            .with_outbox(outbox_dyn, tasks_dyn);

        // Workspace is ALREADY detached (project_id = None) — the state left by
        // a first attempt that flipped/failed partway.
        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws_repo.save(&ws).await.unwrap();

        // A task with a residual stale id AND a residual pending AddItem.
        let task = save_mirror(&tasks, ws.id, Some("I_stale"), Some("PVTI_stale")).await;
        let stale_add = domain_sync::OutboxEntry::new(
            task.id,
            OutboxMutation::AddItem {
                project_node_id: project.id.as_str().to_string(),
                issue_node_id: "I_stale".into(),
            },
        );
        outbox.enqueue(&stale_add).await.unwrap();

        // Retry the detach: even though the workspace is already detached, the
        // scrub must complete — clear the stale id and cancel the pending Add.
        svc.set_project(&ws.id.to_string(), None).await.unwrap();

        let scrubbed = tasks.get(task.id).await.unwrap();
        assert_eq!(
            scrubbed.project_item_id, None,
            "retry scrubs the residual stale project_item_id even when already detached"
        );
        let pending_adds = outbox
            .all()
            .into_iter()
            .filter(|e| {
                e.task_id == task.id
                    && e.status == domain_sync::OutboxStatus::Pending
                    && e.mutation.kind() == "add_item"
            })
            .count();
        assert_eq!(
            pending_adds, 0,
            "retry cancels the residual pending AddItem so it can't re-anchor after detach"
        );
    }
}
