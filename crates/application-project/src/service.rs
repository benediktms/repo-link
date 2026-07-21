//! [`ProjectService`] — orchestration for `Project` aggregates and the
//! `<project-spec>` resolver.

use std::sync::Arc;

use std::collections::HashSet;

use domain_core::{ProjectId, Timestamp};
use domain_project::{
    FieldOption, PriorityMapping, Project, ProjectField, ProjectFieldKind, StatusMapping,
    assign_field_kinds, derive_priority_mappings, derive_status_mappings,
};
use domain_task::Priority;
use dto_shared::{LinkProjectCmd, MapPriorityCmd, MapStatusCmd, ProjectDto};
use ports::{PortError, ProjectRepository, RemoteProjectSnapshot};

use crate::dto::project_to_dto;
use crate::error::{Result, ServiceError};
use crate::status::parse_status;

/// Outcome of [`ProjectService::link_from_snapshot`]: the linked project plus
/// any plain advisory notes for the CLI to surface on stderr.
///
/// This is the lazy form of RFC 0006 D10 — advisories are plain strings, NOT
/// structured `SyncNoticeDto` variants (those stay unbuilt until a task
/// actually consumes them, #228). Today the only advisory is the priority
/// clamp-collapse note (D3): a board with fewer than four Priority options
/// forces two local priorities onto one board option.
#[derive(Debug, Clone)]
pub struct LinkOutcome {
    pub project: ProjectDto,
    pub advisories: Vec<String>,
}

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
        let status_options: Vec<FieldOption> = cmd
            .status_options
            .into_iter()
            .map(|o| FieldOption {
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
                    is_open: parse_status(&m.status)?,
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
    pub async fn link_from_snapshot(&self, snap: RemoteProjectSnapshot) -> Result<LinkOutcome> {
        let id = ProjectId::parse(snap.node_id)?;
        // Map the retained single-selects into domain fields and classify them
        // by name (RFC 0006 D9) — Status-vs-other selection is a domain concern,
        // not the adapter's. Every field is persisted (genuine round trip), but
        // only the Status field drives lifecycle today.
        let fields: Vec<ProjectField> = snap
            .fields
            .into_iter()
            .map(|f| {
                ProjectField::new(
                    f.field_id,
                    f.name,
                    f.options
                        .into_iter()
                        .map(|o| FieldOption {
                            option_id: o.option_id,
                            name: o.name,
                            ordinal: o.ordinal,
                        })
                        .collect(),
                )
            })
            .collect();
        let fields = assign_field_kinds(fields);

        // Derive the local-status → option mapping from the Status field's
        // catalog. A board with no single-select field can't be driven, so
        // linking it is an error (RFC 0001 §3 D1 / 0006 D9).
        let status_mappings = match fields.iter().find(|f| f.kind == ProjectFieldKind::Status) {
            Some(status) => derive_status_mappings(&status.options),
            None => return Err(ServiceError::NoStatusField(id.as_str().to_string())),
        };

        // Priority mapping (RFC 0006 D3) is opt-in: derive by ordinal ONLY when
        // a Priority field was classified. No Priority field → no mapping, not
        // an error. When the board has fewer than four options the clamp
        // collapses two local priorities onto one board option; surface that as
        // a plain advisory (D10, lazy form) — detected as two distinct
        // priorities sharing an `option_id`.
        let mut advisories = Vec::new();
        let priority_mappings = match fields.iter().find(|f| f.kind == ProjectFieldKind::Priority) {
            Some(priority) => {
                let mappings = derive_priority_mappings(&priority.options);
                let distinct: HashSet<&str> =
                    mappings.iter().map(|m| m.option_id.as_str()).collect();
                if !mappings.is_empty() && distinct.len() < mappings.len() {
                    advisories.push(format!(
                        "Priority clamp: board '{}' exposes {} priority option(s) for 4 local \
                         priorities (P0..P3), so two or more collapse onto one board option \
                         (RFC 0006 D3; edit the mapping to override)",
                        snap.title,
                        distinct.len(),
                    ));
                }
                mappings
            }
            None => Vec::new(),
        };

        let project = Project::from_fields(
            id,
            snap.owner_login,
            snap.number,
            snap.title,
            fields,
            status_mappings,
            priority_mappings,
            false,
            Timestamp::now(),
        )?;
        self.repo.save(&project).await?;
        Ok(LinkOutcome {
            project: project_to_dto(&project),
            advisories,
        })
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

    /// Replace the mapping for one open/closed bucket. If a mapping for the
    /// same bucket existed, it is overwritten; otherwise it is appended. With
    /// only two buckets (open / closed) the collection holds at most two rows.
    pub async fn map_status(&self, cmd: MapStatusCmd) -> Result<ProjectDto> {
        let mut project = resolve_project(&self.repo, &cmd.project_spec).await?;
        let is_open = parse_status(&cmd.status)?;
        if !project
            .status_options()
            .iter()
            .any(|o| o.option_id == cmd.option_id)
        {
            return Err(ServiceError::UnknownOption(
                cmd.option_id,
                project.id.as_str().to_string(),
            ));
        }
        let mut mappings = project.status_mappings.clone();
        if let Some(existing) = mappings.iter_mut().find(|m| m.is_open == is_open) {
            existing.option_id = cmd.option_id;
        } else {
            mappings.push(StatusMapping {
                is_open,
                option_id: cmd.option_id,
            });
        }
        project.set_mappings(mappings, Timestamp::now())?;
        self.repo.save(&project).await?;
        Ok(project_to_dto(&project))
    }

    /// Replace the mapping for one local priority, leaving the other three
    /// buckets untouched.
    pub async fn map_priority(&self, cmd: MapPriorityCmd) -> Result<ProjectDto> {
        let mut project = resolve_project(&self.repo, &cmd.project_spec).await?;
        let priority = match cmd.priority.as_str() {
            "p0" => Priority::P0,
            "p1" => Priority::P1,
            "p2" => Priority::P2,
            "p3" => Priority::P3,
            _ => return Err(ServiceError::UnknownPriority(cmd.priority)),
        };
        if !project
            .priority_options()
            .iter()
            .any(|o| o.option_id == cmd.option_id)
        {
            return Err(ServiceError::UnknownOption(
                cmd.option_id,
                project.id.as_str().to_string(),
            ));
        }
        let mut mappings = project.priority_mappings.clone();
        if let Some(existing) = mappings.iter_mut().find(|m| m.priority == priority) {
            existing.option_id = cmd.option_id;
        } else {
            mappings.push(PriorityMapping {
                priority,
                option_id: cmd.option_id,
            });
        }
        project.set_priority_mappings(mappings, Timestamp::now())?;
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
                status: "closed".into(),
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
        // Two retained single-selects: a Priority field (exercises retention)
        // and the Status field the mapping is derived from.
        RemoteProjectSnapshot {
            node_id: "PVT_snap".into(),
            number: 3,
            title: "repo-link".into(),
            owner_login: "benediktms".into(),
            fields: vec![
                ports::RemoteProjectField {
                    field_id: "PVTSSF_prio".into(),
                    name: "Priority".into(),
                    options: vec![
                        ports::RemoteProjectFieldOption {
                            option_id: "p0".into(),
                            name: "P0".into(),
                            ordinal: 0,
                        },
                        ports::RemoteProjectFieldOption {
                            option_id: "p1".into(),
                            name: "P1".into(),
                            ordinal: 1,
                        },
                    ],
                },
                ports::RemoteProjectField {
                    field_id: "PVTSSF_x".into(),
                    name: "Status".into(),
                    options: vec![
                        ports::RemoteProjectFieldOption {
                            option_id: "f7".into(),
                            name: "Backlog".into(),
                            ordinal: 0,
                        },
                        ports::RemoteProjectFieldOption {
                            option_id: "47".into(),
                            name: "In progress".into(),
                            ordinal: 2,
                        },
                        ports::RemoteProjectFieldOption {
                            option_id: "98".into(),
                            name: "Done".into(),
                            ordinal: 4,
                        },
                    ],
                },
            ],
        }
    }

    #[tokio::test]
    async fn link_from_snapshot_auto_derives_mappings_by_name() {
        let svc = service();
        let dto = svc.link_from_snapshot(snapshot()).await.unwrap().project;
        assert_eq!(dto.id, "PVT_snap");
        assert_eq!(dto.status_options.len(), 3);
        let m = |s: &str| {
            dto.status_mappings
                .iter()
                .find(|x| x.status == s)
                .map(|x| x.option_id.as_str())
        };
        // RFC 0004: at most two rows. Open maps to the first open-like option
        // (Backlog), closed maps to the last closed-like option (Done). The
        // middle "In progress" option is not a derived target.
        assert_eq!(dto.status_mappings.len(), 2);
        assert_eq!(m("open"), Some("f7"));
        assert_eq!(m("closed"), Some("98"));
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

    #[tokio::test]
    async fn link_from_snapshot_retains_non_status_fields() {
        // The Priority field in the snapshot must be persisted alongside the
        // Status field, classified `Priority` — genuine round trip.
        let repo = Arc::new(InMemoryProjectRepository::new());
        let svc = ProjectService::new(repo.clone());
        svc.link_from_snapshot(snapshot()).await.unwrap();

        let project = repo
            .get(ProjectId::parse("PVT_snap".to_string()).unwrap())
            .await
            .unwrap();
        // Both fields retained; the Status field derived the mappings.
        assert_eq!(project.fields.len(), 2);
        assert_eq!(project.status_field_id(), Some("PVTSSF_x"));
        assert!(
            project
                .fields
                .iter()
                .any(|f| f.kind == domain_project::ProjectFieldKind::Priority
                    && f.name == "Priority"
                    && f.options.len() == 2),
            "the Priority field must survive the link round trip: {:?}",
            project.fields
        );
    }

    #[tokio::test]
    async fn link_from_snapshot_without_single_select_errors() {
        let svc = service();
        let mut s = snapshot();
        s.fields = vec![];
        let err = svc.link_from_snapshot(s).await.unwrap_err();
        assert!(matches!(err, ServiceError::NoStatusField(_)), "got {err:?}");
    }

    // ---------- priority mapping derivation at link (RFC 0006 D3) ----------

    /// A snapshot with a Status field (Backlog/Done) plus a Priority field
    /// carrying `prio_options` (in the given order, ordinals 0..N).
    fn snapshot_with_priority(prio_options: &[&str]) -> RemoteProjectSnapshot {
        RemoteProjectSnapshot {
            node_id: "PVT_prio".into(),
            number: 4,
            title: "priority board".into(),
            owner_login: "benediktms".into(),
            fields: vec![
                ports::RemoteProjectField {
                    field_id: "PVTSSF_x".into(),
                    name: "Status".into(),
                    options: vec![
                        ports::RemoteProjectFieldOption {
                            option_id: "s_open".into(),
                            name: "Backlog".into(),
                            ordinal: 0,
                        },
                        ports::RemoteProjectFieldOption {
                            option_id: "s_done".into(),
                            name: "Done".into(),
                            ordinal: 1,
                        },
                    ],
                },
                ports::RemoteProjectField {
                    field_id: "PVTSSF_prio".into(),
                    name: "Priority".into(),
                    options: prio_options
                        .iter()
                        .enumerate()
                        .map(|(i, name)| ports::RemoteProjectFieldOption {
                            option_id: format!("po{i}"),
                            name: (*name).to_string(),
                            ordinal: u32::try_from(i).unwrap(),
                        })
                        .collect(),
                },
            ],
        }
    }

    async fn link_and_load(snap: RemoteProjectSnapshot) -> (Project, Vec<String>) {
        let repo = Arc::new(InMemoryProjectRepository::new());
        let svc = ProjectService::new(repo.clone());
        let id = ProjectId::parse(snap.node_id.clone()).unwrap();
        let outcome = svc.link_from_snapshot(snap).await.unwrap();
        let project = repo.get(id).await.unwrap();
        (project, outcome.advisories)
    }

    #[tokio::test]
    async fn link_derives_priority_mappings_by_ordinal_four_options() {
        // Exact fit: four options → four one-to-one mappings, no clamp advisory.
        let (project, advisories) =
            link_and_load(snapshot_with_priority(&["Urgent", "High", "Medium", "Low"])).await;
        assert_eq!(project.priority_mappings.len(), 4);
        let targets: Vec<&str> = project
            .priority_mappings
            .iter()
            .map(|m| m.option_id.as_str())
            .collect();
        // Ordinal mapping: P0..P3 → po0..po3, distinct.
        assert_eq!(targets, ["po0", "po1", "po2", "po3"]);
        assert!(
            advisories.is_empty(),
            "no clamp on a 4-option board: {advisories:?}"
        );
    }

    #[tokio::test]
    async fn link_clamp_collapse_on_three_options_emits_advisory() {
        // Three options for four priorities → the tail clamps (two priorities
        // share the last option) and a plain advisory is surfaced.
        let (project, advisories) =
            link_and_load(snapshot_with_priority(&["High", "Medium", "Low"])).await;
        assert_eq!(project.priority_mappings.len(), 4);
        let targets: Vec<&str> = project
            .priority_mappings
            .iter()
            .map(|m| m.option_id.as_str())
            .collect();
        // P0→po0, P1→po1, P2→po2, P3 clamps onto po2.
        assert_eq!(targets, ["po0", "po1", "po2", "po2"]);
        assert_eq!(advisories.len(), 1, "one clamp advisory expected");
        assert!(
            advisories[0].contains("Priority clamp"),
            "advisory should be the clamp note, got: {}",
            advisories[0]
        );
    }

    #[tokio::test]
    async fn link_without_priority_field_derives_no_mapping_and_no_advisory() {
        // Opt-in: the base snapshot's Priority field removed → no priority
        // mapping, no advisory, and linking still succeeds.
        let mut s = snapshot();
        s.fields.retain(|f| f.name != "Priority");
        let (project, advisories) = link_and_load(s).await;
        assert!(project.priority_mappings.is_empty());
        assert!(project.priority_field().is_none());
        assert!(advisories.is_empty());
    }
}
