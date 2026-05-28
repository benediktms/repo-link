//! Error types for the project service surface.

use domain_core::ProjectIdParseError;
use ports::PortError;
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
