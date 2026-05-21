//! application-workspace — workspace + repo binding orchestration.

use std::path::PathBuf;
use std::sync::Arc;

use domain_core::{IdParseError, RepoId, WorkspaceId};
use domain_repo::RepoBinding;
use domain_workspace::{Workspace, WorkspaceName};
use dto_shared::{
    AttachRepoCmd, CreateWorkspaceCmd, LinkWorktreeCmd, ListWorkspacesQuery, RepoAttachOutcomeDto,
    RepoBindingDto, UnlinkWorktreeCmd, WorkspaceDto, WorktreeLinkDto,
};
use ports::{FilesystemProbe, PortError, RepoBindingRepository, WorkspaceRepository};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    Port(#[from] PortError),
    #[error(transparent)]
    Domain(#[from] domain_core::DomainError),
    #[error("workspace name already in use: {0}")]
    DuplicateName(String),
    #[error("invalid id: {0}")]
    BadId(String),
}

impl From<IdParseError> for ServiceError {
    fn from(e: IdParseError) -> Self {
        Self::BadId(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, ServiceError>;

// ---------- WorkspaceService ---------------------------------------------

pub struct WorkspaceService {
    repo: Arc<dyn WorkspaceRepository>,
}

impl WorkspaceService {
    pub fn new(repo: Arc<dyn WorkspaceRepository>) -> Self {
        Self { repo }
    }

    pub async fn create(&self, cmd: CreateWorkspaceCmd) -> Result<WorkspaceDto> {
        let name = WorkspaceName::new(&cmd.name)?;
        if self.repo.find_by_name(name.as_str()).await?.is_some() {
            return Err(ServiceError::DuplicateName(name.as_str().to_string()));
        }
        let w = Workspace::new(name, cmd.description, cmd.local_only);
        self.repo.save(&w).await?;
        Ok(workspace_to_dto(&w))
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

// ---------- RepoBindingService -------------------------------------------

pub struct RepoBindingService {
    workspaces: Arc<dyn WorkspaceRepository>,
    bindings: Arc<dyn RepoBindingRepository>,
}

impl RepoBindingService {
    pub fn new(
        workspaces: Arc<dyn WorkspaceRepository>,
        bindings: Arc<dyn RepoBindingRepository>,
    ) -> Self {
        Self {
            workspaces,
            bindings,
        }
    }

    /// Idempotent: if a binding for `canonical_url` already exists in the
    /// workspace, mutate that one and report `merged: true`. Otherwise
    /// create a fresh binding. In either case, when `link_path` is set we
    /// register it as a worktree on the resulting binding before saving.
    ///
    /// The service does NOT verify that `link_path` actually corresponds
    /// to a checkout of `canonical_url`; that's the CLI's responsibility
    /// (so tests and programmatic callers can wire whatever they like
    /// without git running).
    pub async fn attach(&self, cmd: AttachRepoCmd) -> Result<RepoAttachOutcomeDto> {
        let workspace_id: WorkspaceId = cmd.workspace_id.parse()?;
        // Confirm the workspace exists; bubbles up as PortError::NotFound otherwise.
        let _ = self.workspaces.get(workspace_id).await?;

        let (mut binding, merged) = match self
            .bindings
            .find_by_canonical_url(workspace_id, &cmd.canonical_url)
            .await?
        {
            Some(existing) => (existing, true),
            None => {
                let mut b =
                    RepoBinding::new(workspace_id, cmd.remote_url, cmd.canonical_url)?;
                b.tracked_branch = cmd.tracked_branch;
                (b, false)
            }
        };

        let worktree_added = cmd.link_path.map(|path| {
            binding.link_worktree(PathBuf::from(&path), cmd.link_branch);
            path
        });

        self.bindings.save(&binding).await?;
        Ok(RepoAttachOutcomeDto {
            binding: binding_to_dto(&binding),
            merged,
            worktree_added,
        })
    }

    pub async fn detach(&self, id: &str) -> Result<()> {
        let id: RepoId = id.parse()?;
        self.bindings.delete(id).await?;
        Ok(())
    }

    pub async fn show(&self, id: &str) -> Result<RepoBindingDto> {
        let id: RepoId = id.parse()?;
        let b = self.bindings.get(id).await?;
        Ok(binding_to_dto(&b))
    }

    pub async fn list(&self, workspace_id: &str) -> Result<Vec<RepoBindingDto>> {
        let workspace_id: WorkspaceId = workspace_id.parse()?;
        let rows = self.bindings.list_by_workspace(workspace_id).await?;
        Ok(rows.iter().map(binding_to_dto).collect())
    }

    pub async fn link_worktree(&self, cmd: LinkWorktreeCmd) -> Result<RepoBindingDto> {
        let id: RepoId = cmd.repo_id.parse()?;
        let mut binding = self.bindings.get(id).await?;
        binding.link_worktree(PathBuf::from(cmd.path), cmd.branch);
        self.bindings.save(&binding).await?;
        Ok(binding_to_dto(&binding))
    }

    pub async fn unlink_worktree(&self, cmd: UnlinkWorktreeCmd) -> Result<RepoBindingDto> {
        let id: RepoId = cmd.repo_id.parse()?;
        let mut binding = self.bindings.get(id).await?;
        binding.unlink_worktree(std::path::Path::new(&cmd.path))?;
        self.bindings.save(&binding).await?;
        Ok(binding_to_dto(&binding))
    }

    pub async fn prune_missing(&self, id: &str) -> Result<RepoBindingDto> {
        let id: RepoId = id.parse()?;
        let mut binding = self.bindings.get(id).await?;
        binding.prune_missing();
        self.bindings.save(&binding).await?;
        Ok(binding_to_dto(&binding))
    }

    /// Walk every binding in the workspace, ask the probe whether each
    /// recorded worktree path still exists, and persist the resulting
    /// status transitions. Optionally prune entries we just marked missing.
    ///
    /// Idempotent — running it twice produces the same final state.
    pub async fn reconcile_worktrees(
        &self,
        workspace_id: &str,
        probe: &dyn FilesystemProbe,
        prune: bool,
    ) -> Result<ReconcileSummary> {
        let workspace_id: WorkspaceId = workspace_id.parse()?;
        // Confirm the workspace exists; bubbles up as PortError::NotFound otherwise.
        let _ = self.workspaces.get(workspace_id).await?;
        let bindings = self.bindings.list_by_workspace(workspace_id).await?;

        let mut summary = ReconcileSummary::default();
        for mut binding in bindings {
            summary.repos_checked += 1;
            let mut missing_paths = Vec::new();
            for link in &binding.worktrees {
                summary.worktrees_checked += 1;
                let exists = probe.path_exists(&link.path).await?;
                let already_missing = matches!(
                    link.status,
                    domain_repo::LinkStatus::MissingPath | domain_repo::LinkStatus::Detached
                );
                if !exists && !already_missing {
                    missing_paths.push(link.path.clone());
                }
            }

            let mut changed = false;
            for path in &missing_paths {
                binding.mark_path_missing(path)?;
                summary.marked_missing += 1;
                changed = true;
            }
            if prune {
                let pruned = binding.prune_missing();
                if pruned > 0 {
                    summary.pruned += pruned;
                    changed = true;
                }
            }
            if changed {
                self.bindings.save(&binding).await?;
            }
        }
        Ok(summary)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileSummary {
    pub repos_checked: usize,
    pub worktrees_checked: usize,
    pub marked_missing: usize,
    pub pruned: usize,
}

// ---------- Mapping (domain → DTO) ---------------------------------------

fn enum_str<T: serde::Serialize>(t: &T) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

pub fn workspace_to_dto(w: &Workspace) -> WorkspaceDto {
    WorkspaceDto {
        id: w.id.to_string(),
        name: w.name.as_str().to_string(),
        description: w.description.clone(),
        status: enum_str(&w.status),
        local_only: w.local_only,
        created_at: w.created_at.into(),
        updated_at: w.updated_at.into(),
    }
}

pub fn binding_to_dto(b: &RepoBinding) -> RepoBindingDto {
    RepoBindingDto {
        id: b.id.to_string(),
        workspace_id: b.workspace_id.to_string(),
        remote_url: b.remote_url.clone(),
        canonical_url: b.canonical_url.clone(),
        tracked_branch: b.tracked_branch.clone(),
        worktrees: b
            .worktrees
            .iter()
            .map(|w| WorktreeLinkDto {
                path: w.path.display().to_string(),
                branch: w.branch.clone(),
                status: enum_str(&w.status),
                last_seen_at: w.last_seen_at.into(),
            })
            .collect(),
        created_at: b.created_at.into(),
        updated_at: b.updated_at.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use testing_fixtures::{InMemoryRepoBindingRepository, InMemoryWorkspaceRepository};

    fn setup() -> (WorkspaceService, RepoBindingService) {
        let workspaces: Arc<dyn WorkspaceRepository> = Arc::new(InMemoryWorkspaceRepository::new());
        let bindings: Arc<dyn RepoBindingRepository> = Arc::new(InMemoryRepoBindingRepository::new());
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
            })
            .await
            .unwrap();
        assert_eq!(dto.status, "created");
        assert_eq!(svc.show(&dto.id).await.unwrap(), dto);
        assert_eq!(
            svc.list(ListWorkspacesQuery::default()).await.unwrap().len(),
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
        })
        .await
        .unwrap();
        let err = svc
            .create(CreateWorkspaceCmd {
                name: "a".into(),
                description: None,
                local_only: true,
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
            })
            .await
            .unwrap();
        let active = svc.activate(&dto.id).await.unwrap();
        assert_eq!(active.status, "active");
        let archived = svc.archive(&dto.id).await.unwrap();
        assert_eq!(archived.status, "archived");
    }

    #[tokio::test]
    async fn attach_repo_requires_workspace() {
        let (_, bsvc) = setup();
        let err = bsvc
            .attach(AttachRepoCmd {
                workspace_id: domain_core::WorkspaceId::new().to_string(),
                remote_url: "git@github.com:o/r.git".into(),
                canonical_url: "github.com/o/r".into(),
                tracked_branch: None,
                link_path: None,
                link_branch: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::Port(PortError::NotFound(_))));
    }

    #[tokio::test]
    async fn attach_and_link_worktree_roundtrip() {
        let (ws_svc, bsvc) = setup();
        let ws = ws_svc
            .create(CreateWorkspaceCmd {
                name: "w".into(),
                description: None,
                local_only: true,
            })
            .await
            .unwrap();
        let b = bsvc
            .attach(AttachRepoCmd {
                workspace_id: ws.id.clone(),
                remote_url: "git@github.com:o/r.git".into(),
                canonical_url: "github.com/o/r".into(),
                tracked_branch: Some("main".into()),
                link_path: None,
                link_branch: None,
            })
            .await
            .unwrap();
        assert!(!b.merged);
        assert!(b.worktree_added.is_none());
        let linked = bsvc
            .link_worktree(LinkWorktreeCmd {
                repo_id: b.binding.id.clone(),
                path: "/tmp/repo".into(),
                branch: Some("main".into()),
            })
            .await
            .unwrap();
        assert_eq!(linked.worktrees.len(), 1);
        assert_eq!(linked.worktrees[0].status, "linked");
    }

    #[tokio::test]
    async fn reconcile_marks_missing_and_optionally_prunes() {
        use testing_fixtures::StubFilesystemProbe;
        let (ws_svc, bsvc) = setup();
        let ws = ws_svc
            .create(CreateWorkspaceCmd {
                name: "w".into(),
                description: None,
                local_only: true,
            })
            .await
            .unwrap();
        let b = bsvc
            .attach(AttachRepoCmd {
                workspace_id: ws.id.clone(),
                remote_url: "git@github.com:o/r.git".into(),
                canonical_url: "github.com/o/r".into(),
                tracked_branch: None,
                link_path: None,
                link_branch: None,
            })
            .await
            .unwrap();
        bsvc.link_worktree(LinkWorktreeCmd {
            repo_id: b.binding.id.clone(),
            path: "/tmp/alive".into(),
            branch: None,
        })
        .await
        .unwrap();
        bsvc.link_worktree(LinkWorktreeCmd {
            repo_id: b.binding.id.clone(),
            path: "/tmp/gone".into(),
            branch: None,
        })
        .await
        .unwrap();

        // Probe sees /tmp/alive but not /tmp/gone.
        let probe = StubFilesystemProbe::new().with_path("/tmp/alive");

        let summary = bsvc
            .reconcile_worktrees(&ws.id, &probe, false)
            .await
            .unwrap();
        assert_eq!(summary.repos_checked, 1);
        assert_eq!(summary.worktrees_checked, 2);
        assert_eq!(summary.marked_missing, 1);
        assert_eq!(summary.pruned, 0);

        // Second pass with prune=true drops the missing path.
        let summary2 = bsvc
            .reconcile_worktrees(&ws.id, &probe, true)
            .await
            .unwrap();
        // /tmp/gone is now MissingPath (already_missing branch) → no new marks,
        // but prune removes it.
        assert_eq!(summary2.marked_missing, 0);
        assert_eq!(summary2.pruned, 1);

        let after = bsvc.show(&b.binding.id).await.unwrap();
        assert_eq!(after.worktrees.len(), 1);
        assert_eq!(after.worktrees[0].path, "/tmp/alive");
    }

    #[tokio::test]
    async fn reconcile_worktrees_unknown_workspace_returns_not_found() {
        use testing_fixtures::StubFilesystemProbe;
        let (_, bsvc) = setup();
        let unknown_id = domain_core::WorkspaceId::new().to_string();
        let probe = StubFilesystemProbe::new();
        let err = bsvc
            .reconcile_worktrees(&unknown_id, &probe, false)
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::Port(PortError::NotFound(_))));
    }

    #[tokio::test]
    async fn attach_with_existing_canonical_merges() {
        let (ws_svc, bsvc) = setup();
        let ws = ws_svc
            .create(CreateWorkspaceCmd {
                name: "w".into(),
                description: None,
                local_only: true,
            })
            .await
            .unwrap();
        let cmd = AttachRepoCmd {
            workspace_id: ws.id.clone(),
            remote_url: "git@github.com:o/r.git".into(),
            canonical_url: "github.com/o/r".into(),
            tracked_branch: None,
            link_path: None,
            link_branch: None,
        };
        let first = bsvc.attach(cmd.clone()).await.unwrap();
        assert!(!first.merged);
        let second = bsvc.attach(cmd).await.unwrap();
        assert!(second.merged);
        assert_eq!(second.binding.id, first.binding.id);
    }

    #[tokio::test]
    async fn attach_merges_when_canonical_exists() {
        let (ws_svc, bsvc) = setup();
        let ws = ws_svc
            .create(CreateWorkspaceCmd {
                name: "w2".into(),
                description: None,
                local_only: true,
            })
            .await
            .unwrap();
        let first = bsvc
            .attach(AttachRepoCmd {
                workspace_id: ws.id.clone(),
                remote_url: "git@github.com:o/r.git".into(),
                canonical_url: "github.com/o/r".into(),
                tracked_branch: None,
                link_path: None,
                link_branch: None,
            })
            .await
            .unwrap();
        assert!(!first.merged);
        assert!(first.worktree_added.is_none());
        assert_eq!(first.binding.worktrees.len(), 0);

        let second = bsvc
            .attach(AttachRepoCmd {
                workspace_id: ws.id.clone(),
                remote_url: "git@github.com:o/r.git".into(),
                canonical_url: "github.com/o/r".into(),
                tracked_branch: None,
                link_path: Some("/tmp/second".into()),
                link_branch: None,
            })
            .await
            .unwrap();
        assert!(second.merged);
        assert_eq!(second.worktree_added, Some("/tmp/second".into()));
        assert_eq!(second.binding.id, first.binding.id);
        assert_eq!(second.binding.worktrees.len(), 1);
    }

    #[tokio::test]
    async fn attach_links_worktree_when_link_path_given() {
        let (ws_svc, bsvc) = setup();
        let ws = ws_svc
            .create(CreateWorkspaceCmd {
                name: "w3".into(),
                description: None,
                local_only: true,
            })
            .await
            .unwrap();
        let outcome = bsvc
            .attach(AttachRepoCmd {
                workspace_id: ws.id.clone(),
                remote_url: "git@github.com:o/r.git".into(),
                canonical_url: "github.com/o/r".into(),
                tracked_branch: None,
                link_path: Some("/tmp/checkout".into()),
                link_branch: Some("main".into()),
            })
            .await
            .unwrap();
        assert!(!outcome.merged);
        assert_eq!(outcome.worktree_added, Some("/tmp/checkout".into()));
        assert_eq!(outcome.binding.worktrees.len(), 1);
        assert_eq!(outcome.binding.worktrees[0].path, "/tmp/checkout");
        assert_eq!(outcome.binding.worktrees[0].branch, Some("main".into()));
        assert_eq!(outcome.binding.worktrees[0].status, "linked");
    }
}
