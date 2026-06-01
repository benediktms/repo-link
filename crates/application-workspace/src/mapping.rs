//! Mapping (domain → DTO) plus the prefix-conflict translation helper.

use domain_repo::RepoBinding;
use domain_workspace::Workspace;
use dto_shared::{RepoBindingDto, WorkspaceDto, WorktreeLinkDto};
use ports::PortError;

use crate::error::ServiceError;

/// Translate a `repos.prefix` UNIQUE violation into the friendly
/// [`ServiceError::PrefixTaken`]; pass every other error through
/// unchanged. Used on the explicit-prefix paths (`attach --prefix`
/// and `set_prefix`) where we want the user to pick a different value
/// rather than see a raw SQL message or get silent suffix-bumping.
pub(crate) fn map_prefix_conflict(e: PortError, prefix: &str) -> ServiceError {
    if e.conflict_target() == Some("repos.prefix") {
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

pub fn binding_to_dto(b: &RepoBinding) -> RepoBindingDto {
    RepoBindingDto {
        id: b.id.to_string(),
        workspace_id: b.workspace_id.to_string(),
        remote_url: b.remote_url.clone(),
        canonical_url: b.canonical_url.clone(),
        tracked_branch: b.tracked_branch.clone(),
        name: b.name.clone(),
        aliases: b.aliases.clone(),
        prefix: b.prefix.clone(),
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
