//! [`ProjectService`] — orchestration for `Project` aggregates and the
//! `<project-spec>` resolver.

use std::sync::Arc;

use domain_core::{ProjectId, Timestamp};
use domain_project::{Project, StatusMapping, StatusOption, derive_status_mappings};
use dto_shared::{LinkProjectCmd, MapStatusCmd, ProjectDto};
use ports::{PortError, ProjectRepository, RemoteProjectSnapshot};

use crate::dto::project_to_dto;
use crate::error::{Result, ServiceError};
use crate::status::parse_status;

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
        // Normalize "id parses but no row exists" to ProjectNotFound so
        // callers can match on one variant regardless of input form.
        return repo.get(id).await.map_err(|e| match e {
            PortError::NotFound(_) => ServiceError::ProjectNotFound(spec.to_string()),
            other => ServiceError::Port(other),
        });
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

pub struct ProjectService {
    repo: Arc<dyn ProjectRepository>,
}

impl ProjectService {
    pub fn new(repo: Arc<dyn ProjectRepository>) -> Self {
        Self { repo }
    }

    /// Link a project from a hand-entered schema. This is a lower-level
    /// programmatic seam (used by tests and available for future import
    /// tooling); the CLI links via [`Self::link_from_snapshot`] with a
    /// GraphQL-fetched schema instead.
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

    /// Link a project from a freshly-fetched remote snapshot (Stage 5).
    ///
    /// The CLI resolves `owner/number` over GraphQL via
    /// [`ports::RemoteProjectProvider::fetch_project`] and hands the snapshot
    /// here; we auto-derive the local-status → option mapping by option name
    /// (RFC 0001 §3) and persist. Re-linking an existing project refreshes
    /// its schema and re-seeds the mapping — `save` is an upsert keyed on the
    /// node id.
    pub async fn link_from_snapshot(&self, snap: RemoteProjectSnapshot) -> Result<ProjectDto> {
        let id = ProjectId::parse(snap.node_id)?;
        let status_options: Vec<StatusOption> = snap
            .status_options
            .into_iter()
            .map(|o| StatusOption {
                option_id: o.option_id,
                name: o.name,
                ordinal: o.ordinal,
            })
            .collect();
        let status_mappings = derive_status_mappings(&status_options);
        let project = Project::new(
            id,
            snap.owner_login,
            snap.number,
            snap.title,
            snap.status_field_id,
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
    use dto_shared::{StatusMappingDto, StatusOptionDto};
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
    async fn get_errors_consistently_on_unknown_node_id() {
        // Same logical failure as `owner/number` missing should surface as
        // ServiceError::ProjectNotFound regardless of input form — otherwise
        // callers pattern-matching on the variant miss the node-id path.
        let svc = service();
        let err = svc.get("PVT_does_not_exist").await.unwrap_err();
        assert!(
            matches!(err, ServiceError::ProjectNotFound(_)),
            "expected ProjectNotFound, got: {err:?}"
        );
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

    fn snapshot() -> RemoteProjectSnapshot {
        RemoteProjectSnapshot {
            node_id: "PVT_snap".into(),
            number: 3,
            title: "repo-link".into(),
            owner_login: "benediktms".into(),
            status_field_id: "PVTSSF_x".into(),
            status_options: vec![
                ports::RemoteProjectStatusOption {
                    option_id: "f7".into(),
                    name: "Backlog".into(),
                    ordinal: 0,
                },
                ports::RemoteProjectStatusOption {
                    option_id: "47".into(),
                    name: "In progress".into(),
                    ordinal: 2,
                },
                ports::RemoteProjectStatusOption {
                    option_id: "98".into(),
                    name: "Done".into(),
                    ordinal: 4,
                },
            ],
        }
    }

    #[tokio::test]
    async fn link_from_snapshot_auto_derives_mappings_by_name() {
        let svc = service();
        let dto = svc.link_from_snapshot(snapshot()).await.unwrap();
        assert_eq!(dto.id, "PVT_snap");
        assert_eq!(dto.status_options.len(), 3);
        let m = |s: &str| {
            dto.status_mappings
                .iter()
                .find(|x| x.status == s)
                .map(|x| x.option_id.as_str())
        };
        // Backlog→open, In progress→in_progress, Done→done; no Blocked option
        // on this board, so blocked stays unmapped.
        assert_eq!(m("open"), Some("f7"));
        assert_eq!(m("in_progress"), Some("47"));
        assert_eq!(m("done"), Some("98"));
        assert_eq!(m("blocked"), None);
    }

    #[tokio::test]
    async fn link_from_snapshot_is_resolvable_by_owner_number() {
        let svc = service();
        svc.link_from_snapshot(snapshot()).await.unwrap();
        let dto = svc.get("benediktms/3").await.unwrap();
        assert_eq!(dto.id, "PVT_snap");
    }

    #[tokio::test]
    async fn link_from_snapshot_rejects_non_pvt_node_id() {
        let svc = service();
        let mut s = snapshot();
        s.node_id = "not-a-node-id".into();
        let err = svc.link_from_snapshot(s).await.unwrap_err();
        assert!(matches!(err, ServiceError::BadProjectId(_)));
    }
}
