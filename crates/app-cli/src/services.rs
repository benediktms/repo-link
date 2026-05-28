//! Service container + bootstrap, plus the shared GitHub provider / token /
//! sync-service helpers. `Services` holds every application service the
//! dispatch modules read; `bootstrap` wires the SQLite repositories into it.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use application_project::ProjectService;
use application_query::QueryService;
use application_sync::SyncService;
use application_task::TaskService;
use application_workspace::{RepoBindingService, WorkspaceService};
use infra_config::RepoLinkConfig;
use infra_github::GithubTaskProvider;
use infra_sqlite::{
    SqliteProjectRepository, SqliteRepoBindingRepository, SqliteTaskRepository,
    SqliteTaskSnapshotRepository, SqliteWorkspaceRepository, open_from_path,
};

pub(crate) struct Services {
    pub(crate) workspaces: WorkspaceService,
    pub(crate) bindings: RepoBindingService,
    pub(crate) tasks: TaskService,
    pub(crate) query: QueryService,
    pub(crate) projects: ProjectService,
    pub(crate) tasks_repo: Arc<dyn ports::TaskRepository>,
    pub(crate) bindings_repo: Arc<dyn ports::RepoBindingRepository>,
}

pub(crate) async fn bootstrap(cfg: &RepoLinkConfig) -> Result<Services> {
    let db = open_from_path(&cfg.database_path).await?;
    let workspaces_repo: Arc<dyn ports::WorkspaceRepository> =
        Arc::new(SqliteWorkspaceRepository::new(db.clone()));
    let bindings_repo: Arc<dyn ports::RepoBindingRepository> =
        Arc::new(SqliteRepoBindingRepository::new(db.clone()));
    let tasks_repo: Arc<dyn ports::TaskRepository> =
        Arc::new(SqliteTaskRepository::new(db.clone()));
    let snapshots_repo: Arc<dyn ports::TaskSnapshotRepository> =
        Arc::new(SqliteTaskSnapshotRepository::new(db.clone()));
    let projects_repo: Arc<dyn ports::ProjectRepository> =
        Arc::new(SqliteProjectRepository::new(db));

    Ok(Services {
        workspaces: WorkspaceService::with_projects(workspaces_repo.clone(), projects_repo.clone()),
        bindings: RepoBindingService::new(workspaces_repo.clone(), bindings_repo.clone()),
        tasks: TaskService::new(tasks_repo.clone(), snapshots_repo, bindings_repo.clone()),
        query: QueryService::new(workspaces_repo, bindings_repo.clone(), tasks_repo.clone()),
        projects: ProjectService::new(projects_repo),
        tasks_repo,
        bindings_repo,
    })
}

/// Construct a `GithubTaskProvider`, honoring `REPO_LINK_GITHUB_API_BASE_URL`
/// when set (for GitHub Enterprise or integration tests pointing at a
/// wiremock). Falls back to api.github.com.
pub(crate) fn build_github_provider(
    token: &str,
    cfg: &RepoLinkConfig,
) -> Result<GithubTaskProvider, ports::PortError> {
    match cfg.github_api_base_url.as_deref() {
        Some(url) => GithubTaskProvider::with_base_url(token, url),
        None => GithubTaskProvider::new(token),
    }
}

/// Resolve the GitHub token or fail with a command-specific "set token or
/// run `rl gh auth`" message. Centralised so the wording — including the
/// resolved token-file path — stays in one place.
pub(crate) fn require_github_token(cfg: &RepoLinkConfig, verb: &str) -> Result<String> {
    cfg.resolve_github_token()
        .map_err(|e| anyhow!("{e}"))?
        .ok_or_else(|| {
            anyhow!(
                "{verb} requires REPO_LINK_GITHUB_TOKEN or GITHUB_TOKEN to be set, \
                 or a token file at {} (write one with `rl gh auth`)",
                cfg.token_file_path.display()
            )
        })
}

/// Build a [`SyncService`] wired to a GitHub provider for the current
/// config. `verb` is interpolated into the "no token" error so a missing
/// token reports against the actual verb the user typed (`sync push`,
/// `task link`, `task claim`, …).
pub(crate) fn build_sync_service(
    cfg: &RepoLinkConfig,
    svc: &Services,
    verb: &str,
) -> Result<SyncService> {
    let token = require_github_token(cfg, verb)?;
    let provider: Arc<dyn ports::RemoteTaskProvider> =
        Arc::new(build_github_provider(&token, cfg).map_err(|e| anyhow!("{e}"))?);
    Ok(SyncService::new(
        svc.tasks_repo.clone(),
        svc.bindings_repo.clone(),
        provider,
    ))
}
