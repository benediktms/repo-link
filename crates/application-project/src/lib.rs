//! application-project — orchestration for `Project` aggregates and the
//! workspace ↔ project link surface (RFC 0001 Stage 4).
//!
//! All operations are local-only: project schema is hand-entered through
//! [`ProjectService::link`] and never fetched from GitHub. Stage 5 swaps
//! the GraphQL adapter in behind the same [`LinkProjectCmd`] shape so the
//! service surface doesn't change.

use std::sync::Arc;

use domain_core::{ProjectId, ProjectIdParseError, Timestamp};
use domain_project::{Project, StatusMapping, StatusOption};
use domain_task::TaskStatus;
use dto_shared::{LinkProjectCmd, MapStatusCmd, ProjectDto, StatusMappingDto, StatusOptionDto};
use ports::{PortError, ProjectRepository};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    Port(#[from] PortError),
    #[error(transparent)]
    Domain(#[from] domain_core::DomainError),
    #[error("invalid project id: {0}")]
    BadProjectId(String),
    #[error("project not found: no match for '{0}'")]
    ProjectNotFound(String),
    #[error("ambiguous project spec '{0}': {count} projects match", count = .1)]
    AmbiguousSpec(String, usize),
    #[error("unknown task status '{0}'")]
    UnknownStatus(String),
    #[error("option_id '{0}' is not part of project '{1}'")]
    UnknownOption(String, String),
}

impl From<ProjectIdParseError> for ServiceError {
    fn from(e: ProjectIdParseError) -> Self {
        Self::BadProjectId(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, ServiceError>;

/// `<project-spec>` resolver. Accepts either a `PVT_…` node id directly or
/// `owner/number`. The `owner/number` path scans `list_all` because we
/// don't index that pair (projects are addressed by node id everywhere
/// downstream); for an `rl`-scale install this is N=few-dozen and trivial.
async fn resolve_project(repo: &Arc<dyn ProjectRepository>, spec: &str) -> Result<Project> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(ServiceError::ProjectNotFound(spec.to_string()));
    }
    // Try node id first — cheap O(1) lookup and the canonical form.
    if let Ok(id) = ProjectId::parse(trimmed.to_string()) {
        return Ok(repo.get(id).await?);
    }
    // Fall back to `owner/number`. Reject anything else with a clear error.
    let (owner, number_str) = trimmed
        .split_once('/')
        .ok_or_else(|| ServiceError::ProjectNotFound(spec.to_string()))?;
    let number: u64 = number_str
        .parse()
        .map_err(|_| ServiceError::ProjectNotFound(spec.to_string()))?;
    let all = repo.list_all().await?;
    let mut matches: Vec<Project> = all
        .into_iter()
        .filter(|p| p.owner_login == owner && p.number == number)
        .collect();
    match matches.len() {
        0 => Err(ServiceError::ProjectNotFound(spec.to_string())),
        1 => Ok(matches.remove(0)),
        // Same (owner, number) twice locally would mean someone linked the
        // same project under two different node ids — impossible against
        // GitHub but worth surfacing as an explicit error rather than a
        // random pick.
        n => Err(ServiceError::AmbiguousSpec(spec.to_string(), n)),
    }
}

fn parse_status(raw: &str) -> Result<TaskStatus> {
    // `Archived` is intentionally not accepted: the schema CHECK on
    // `project_status_options.default_for` only allows the four
    // workflow-visible statuses, and an archived task is hidden from sync
    // anyway. Mapping it would never have an effect.
    match raw {
        "open" => Ok(TaskStatus::Open),
        "in_progress" => Ok(TaskStatus::InProgress),
        "blocked" => Ok(TaskStatus::Blocked),
        "done" => Ok(TaskStatus::Done),
        other => Err(ServiceError::UnknownStatus(other.to_string())),
    }
}

fn status_to_str(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Open => "open",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Done => "done",
        // Domain permits the variant but the schema's CHECK rejects it on
        // save. We never construct a mapping carrying it (see `parse_status`)
        // so reaching this arm signals a corrupt local mapping — surface
        // it as the literal so the load-time `Project::new` validator can
        // bubble it as a domain error rather than a panic.
        TaskStatus::Archived => "archived",
    }
}

fn project_to_dto(p: &Project) -> ProjectDto {
    let mut options: Vec<StatusOptionDto> = p
        .status_options
        .iter()
        .map(|o| {
            let default_for = p
                .status_mappings
                .iter()
                .find(|m| m.option_id == o.option_id)
                .map(|m| status_to_str(m.status).to_string());
            StatusOptionDto {
                option_id: o.option_id.clone(),
                name: o.name.clone(),
                ordinal: o.ordinal,
                default_for,
            }
        })
        .collect();
    options.sort_by_key(|o| o.ordinal);
    ProjectDto {
        id: p.id.as_str().to_string(),
        owner_login: p.owner_login.clone(),
        number: p.number,
        title: p.title.clone(),
        status_field_id: p.status_field_id.clone(),
        status_options: options,
        status_mappings: p
            .status_mappings
            .iter()
            .map(|m| StatusMappingDto {
                status: status_to_str(m.status).to_string(),
                option_id: m.option_id.clone(),
            })
            .collect(),
        archived: p.archived,
        created_at: p.created_at.into_inner(),
        updated_at: p.updated_at.into_inner(),
    }
}

// ---------- Service --------------------------------------------------------

pub struct ProjectService {
    repo: Arc<dyn ProjectRepository>,
}

impl ProjectService {
    pub fn new(repo: Arc<dyn ProjectRepository>) -> Self {
        Self { repo }
    }

    /// Link a project from hand-entered schema (Stage 4). Stage 5 will
    /// rewire the CLI to fetch the same shape from GitHub.
    pub async fn link(&self, cmd: LinkProjectCmd) -> Result<ProjectDto> {
        let id = ProjectId::parse(cmd.node_id.clone())?;
        let status_options: Vec<StatusOption> = cmd
            .status_options
            .into_iter()
            .map(|o| StatusOption {
                option_id: o.option_id,
                name: o.name,
                ordinal: o.ordinal,
            })
            .collect();
        let status_mappings: Vec<StatusMapping> = cmd
            .initial_mappings
            .into_iter()
            .map(|m| {
                Ok(StatusMapping {
                    status: parse_status(&m.status)?,
                    option_id: m.option_id,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let project = Project::new(
            id,
            cmd.owner_login,
            cmd.number,
            cmd.title,
            cmd.status_field_id,
            status_options,
            status_mappings,
            false,
            Timestamp::now(),
        )?;
        self.repo.save(&project).await?;
        Ok(project_to_dto(&project))
    }

    pub async fn get(&self, spec: &str) -> Result<ProjectDto> {
        let project = resolve_project(&self.repo, spec).await?;
        Ok(project_to_dto(&project))
    }

    pub async fn list(&self) -> Result<Vec<ProjectDto>> {
        let projects = self.repo.list_all().await?;
        Ok(projects.iter().map(project_to_dto).collect())
    }

    pub async fn unlink(&self, spec: &str) -> Result<()> {
        let project = resolve_project(&self.repo, spec).await?;
        self.repo.delete(project.id).await?;
        Ok(())
    }

    /// Replace the mapping for one local `TaskStatus`. If a mapping for the
    /// same status existed, it is overwritten; otherwise it is appended.
    /// Many-to-one mappings (multiple statuses → same option) are valid in
    /// the domain but a known storage limitation today — see issue #80.
    pub async fn map_status(&self, cmd: MapStatusCmd) -> Result<ProjectDto> {
        let mut project = resolve_project(&self.repo, &cmd.project_spec).await?;
        let status = parse_status(&cmd.status)?;
        if !project
            .status_options
            .iter()
            .any(|o| o.option_id == cmd.option_id)
        {
            return Err(ServiceError::UnknownOption(
                cmd.option_id,
                project.id.as_str().to_string(),
            ));
        }
        let mut mappings = project.status_mappings.clone();
        if let Some(existing) = mappings.iter_mut().find(|m| m.status == status) {
            existing.option_id = cmd.option_id;
        } else {
            mappings.push(StatusMapping {
                status,
                option_id: cmd.option_id,
            });
        }
        project.set_mappings(mappings, Timestamp::now())?;
        self.repo.save(&project).await?;
        Ok(project_to_dto(&project))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use testing_fixtures::InMemoryProjectRepository;

    fn link_cmd() -> LinkProjectCmd {
        LinkProjectCmd {
            node_id: "PVT_test_abc".into(),
            owner_login: "acme".into(),
            number: 7,
            title: "Repo Link".into(),
            status_field_id: "PVTSSF_x".into(),
            status_options: vec![
                StatusOptionDto {
                    option_id: "o1".into(),
                    name: "Backlog".into(),
                    ordinal: 0,
                    default_for: None,
                },
                StatusOptionDto {
                    option_id: "o2".into(),
                    name: "Done".into(),
                    ordinal: 1,
                    default_for: None,
                },
            ],
            initial_mappings: vec![StatusMappingDto {
                status: "open".into(),
                option_id: "o1".into(),
            }],
        }
    }

    fn service() -> ProjectService {
        ProjectService::new(Arc::new(InMemoryProjectRepository::new()))
    }

    #[tokio::test]
    async fn link_persists_and_dto_surfaces_mapping_on_options() {
        let svc = service();
        let dto = svc.link(link_cmd()).await.unwrap();
        assert_eq!(dto.id, "PVT_test_abc");
        assert_eq!(dto.owner_login, "acme");
        assert_eq!(dto.status_mappings.len(), 1);
        // The Backlog option in `status_options` should advertise the
        // mapping inline as `default_for = "open"` so a single CLI render
        // shows the relationship without a join.
        let backlog = dto
            .status_options
            .iter()
            .find(|o| o.option_id == "o1")
            .unwrap();
        assert_eq!(backlog.default_for.as_deref(), Some("open"));
    }

    #[tokio::test]
    async fn link_rejects_non_pvt_node_id() {
        let svc = service();
        let mut cmd = link_cmd();
        cmd.node_id = "not-a-node-id".into();
        let err = svc.link(cmd).await.unwrap_err();
        assert!(matches!(err, ServiceError::BadProjectId(_)));
    }

    #[tokio::test]
    async fn get_resolves_owner_number() {
        let svc = service();
        svc.link(link_cmd()).await.unwrap();
        let dto = svc.get("acme/7").await.unwrap();
        assert_eq!(dto.id, "PVT_test_abc");
    }

    #[tokio::test]
    async fn get_resolves_node_id() {
        let svc = service();
        svc.link(link_cmd()).await.unwrap();
        let dto = svc.get("PVT_test_abc").await.unwrap();
        assert_eq!(dto.id, "PVT_test_abc");
    }

    #[tokio::test]
    async fn get_errors_on_unknown_owner_number() {
        let svc = service();
        let err = svc.get("noone/99").await.unwrap_err();
        assert!(matches!(err, ServiceError::ProjectNotFound(_)));
    }

    #[tokio::test]
    async fn map_status_overwrites_existing_mapping() {
        let svc = service();
        svc.link(link_cmd()).await.unwrap();
        // Initial mapping is open → o1. Overwrite with open → o2.
        let dto = svc
            .map_status(MapStatusCmd {
                project_spec: "acme/7".into(),
                status: "open".into(),
                option_id: "o2".into(),
            })
            .await
            .unwrap();
        assert_eq!(dto.status_mappings.len(), 1);
        assert_eq!(dto.status_mappings[0].option_id, "o2");
    }

    #[tokio::test]
    async fn map_status_appends_when_status_unmapped() {
        let svc = service();
        svc.link(link_cmd()).await.unwrap();
        let dto = svc
            .map_status(MapStatusCmd {
                project_spec: "acme/7".into(),
                status: "done".into(),
                option_id: "o2".into(),
            })
            .await
            .unwrap();
        assert_eq!(dto.status_mappings.len(), 2);
    }

    #[tokio::test]
    async fn map_status_rejects_option_not_in_catalog() {
        let svc = service();
        svc.link(link_cmd()).await.unwrap();
        let err = svc
            .map_status(MapStatusCmd {
                project_spec: "acme/7".into(),
                status: "open".into(),
                option_id: "ghost".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::UnknownOption(_, _)));
    }

    #[tokio::test]
    async fn unlink_removes_the_project() {
        let svc = service();
        svc.link(link_cmd()).await.unwrap();
        svc.unlink("acme/7").await.unwrap();
        let err = svc.get("acme/7").await.unwrap_err();
        assert!(matches!(err, ServiceError::ProjectNotFound(_)));
    }

    #[tokio::test]
    async fn list_returns_known_projects_sorted() {
        let svc = service();
        svc.link(link_cmd()).await.unwrap();
        let mut other = link_cmd();
        other.node_id = "PVT_other".into();
        other.owner_login = "zeta".into();
        other.number = 1;
        svc.link(other).await.unwrap();
        let listed = svc.list().await.unwrap();
        assert_eq!(listed.len(), 2);
        // Sort is (owner, number) — `acme` < `zeta`.
        assert_eq!(listed[0].owner_login, "acme");
        assert_eq!(listed[1].owner_login, "zeta");
    }
}
