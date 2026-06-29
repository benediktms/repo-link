//! Mapping (domain → DTO) plus the prefix-conflict translation helper.

use domain_repo::{RepoInstance, RepoOrigin};
use domain_workspace::Workspace;
use dto_shared::{RepoBindingDto, WorkspaceDto, WorktreeLinkDto};
use ports::PortError;

use crate::error::ServiceError;

/// Translate a `repo_origins.prefix` UNIQUE violation into the friendly
/// [`ServiceError::PrefixTaken`]; pass every other error through
/// unchanged. Used on the explicit-prefix paths (`attach --prefix`
/// and `set_prefix`) where we want the user to pick a different value
/// rather than see a raw SQL message or get silent suffix-bumping.
pub(crate) fn map_prefix_conflict(e: PortError, prefix: &str) -> ServiceError {
    if e.conflict_target() == Some("repo_origins.prefix") {
        ServiceError::PrefixTaken(prefix.to_string())
    } else {
        ServiceError::Port(e)
    }
}

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
        project_id: w.project_id.as_ref().map(|p| p.as_str().to_string()),
        filing_repo_id: w.filing_repo_id.map(|r| r.to_string()),
        created_at: w.created_at.into(),
        updated_at: w.updated_at.into(),
    }
}

pub fn binding_to_dto(instance: &RepoInstance, origin: &RepoOrigin) -> RepoBindingDto {
    RepoBindingDto {
        id: instance.id.to_string(),
        workspace_id: instance.workspace_id.to_string(),
        origin_id: origin.id.to_string(),
        remote_url: origin.remote_url.clone(),
        canonical_url: instance.canonical_url.clone(),
        tracked_branch: instance.tracked_branch.clone(),
        name: origin.name.clone(),
        aliases: origin.aliases.clone(),
        prefix: origin.prefix.clone(),
        worktrees: instance
            .worktrees
            .iter()
            .map(|w| WorktreeLinkDto {
                path: w.path.display().to_string(),
                branch: w.branch.clone(),
                status: enum_str(&w.status),
                last_seen_at: w.last_seen_at.into(),
            })
            .collect(),
        created_at: instance.created_at.into(),
        // A change to the shared origin (rename / alias / prefix) should surface
        // as an update too, so reflect whichever side changed most recently.
        updated_at: instance.updated_at.max(origin.updated_at).into(),
    }
}
