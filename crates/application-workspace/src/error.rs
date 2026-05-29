//! Service-layer error types for workspace + repo binding orchestration.

use domain_core::IdParseError;
use ports::PortError;
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
    #[error("project not found: no match for '{0}'")]
    ProjectNotFound(String),
    #[error(
        "workspace is already attached to project '{current}'; reassigning to '{requested}' is not supported \
         (detach with `--project none` first — moving board items between projects needs a migration path)"
    )]
    ProjectReassignmentUnsupported { current: String, requested: String },
    #[error(
        "project ops require a configured ProjectRepository (use WorkspaceService::with_projects)"
    )]
    ProjectsUnconfigured,
    #[error("binding not found: no match for '{0}'")]
    BindingNotFound(String),
    #[error("prefix '{0}' is already taken by another binding — pick a different one")]
    PrefixTaken(String),
    #[error("ambiguous handle '{query}': matched {count} bindings", count = candidates.len())]
    AmbiguousHandle {
        query: String,
        candidates: Vec<AmbiguousCandidate>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmbiguousCandidate {
    pub id: String,
    pub workspace_id: String,
    pub canonical_url: String,
    pub name: String,
}

impl From<IdParseError> for ServiceError {
    fn from(e: IdParseError) -> Self {
        Self::BadId(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, ServiceError>;
