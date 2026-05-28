//! [`WorkspaceService`] — workspace lifecycle + project-attach orchestration.

use std::sync::Arc;

use domain_core::{ProjectId, WorkspaceId};
use domain_workspace::{Workspace, WorkspaceName};
use dto_shared::{CreateWorkspaceCmd, ListWorkspacesQuery, WorkspaceDto};
use ports::{PortError, ProjectRepository, WorkspaceRepository};

use crate::error::{Result, ServiceError};
use crate::mapping::workspace_to_dto;

pub struct WorkspaceService {
    repo: Arc<dyn WorkspaceRepository>,
    /// Optional `ProjectRepository` for the project-aware methods (`create`
    /// with `project_spec`, `set_project`). Callers that never need them —
    /// the daemon's internal services, most tests — wire only the workspace
    /// repo via `new`; the CLI wires both via `with_projects`.
    projects: Option<Arc<dyn ProjectRepository>>,
}

impl WorkspaceService {
    pub fn new(repo: Arc<dyn WorkspaceRepository>) -> Self {
        Self {
            repo,
            projects: None,
        }
    }

    pub fn with_projects(
        repo: Arc<dyn WorkspaceRepository>,
        projects: Arc<dyn ProjectRepository>,
    ) -> Self {
        Self {
            repo,
            projects: Some(projects),
        }
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

    /// Attach (`Some`) or detach (`None`) a workspace from a project.
    /// Resolution accepts a `PVT_…` node id or `owner/number`.
    pub async fn set_project(
        &self,
        workspace_id: &str,
        project_spec: Option<&str>,
    ) -> Result<WorkspaceDto> {
        let id: WorkspaceId = workspace_id.parse()?;
        let mut w = self.repo.get(id).await?;
        w.project_id = match project_spec {
            Some(spec) => Some(self.resolve_project(spec).await?),
            None => None,
        };
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
    }
}
