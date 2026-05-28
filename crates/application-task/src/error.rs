//! Error types for task service operations.

use domain_core::IdParseError;
use ports::PortError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    Port(#[from] PortError),
    #[error(transparent)]
    Domain(#[from] domain_core::DomainError),
    #[error("invalid id: {0}")]
    BadId(String),
    #[error("invalid enum value for {field}: {value}")]
    BadEnum { field: &'static str, value: String },
    /// Composite ID input named one prefix but the task's repo carries
    /// a different one. The bare hash is unique, so we *could* resolve
    /// it silently; the spec explicitly rejects that path because the
    /// mismatch usually indicates a stale copy-paste from another
    /// repo's context.
    #[error(
        "prefix mismatch: input '{input_prefix}-{hash}' but task {hash} lives under prefix '{actual_prefix}'"
    )]
    PrefixMismatch {
        input_prefix: String,
        actual_prefix: String,
        hash: String,
    },
}

impl From<IdParseError> for ServiceError {
    fn from(e: IdParseError) -> Self {
        Self::BadId(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, ServiceError>;
