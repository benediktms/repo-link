//! application-workspace — workspace + repo binding orchestration.

mod error;
mod mapping;
mod repo_binding_service;
mod workspace_service;

pub use error::{AmbiguousCandidate, Result, ServiceError};
pub use mapping::{binding_to_dto, workspace_to_dto};
pub use repo_binding_service::{ReconcileSummary, RepoBindingService};
pub use workspace_service::WorkspaceService;
