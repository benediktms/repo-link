//! [`SyncError`] and the crate's [`Result`] alias.

use domain_core::IdParseError;
use ports::PortError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error(transparent)]
    Port(#[from] PortError),
    #[error(transparent)]
    Domain(#[from] domain_core::DomainError),
    #[error("invalid id: {0}")]
    BadId(String),
    #[error("task is not bound to a repo")]
    NoRepo,
    #[error("task has no remote reference; promote it first")]
    NoRemote,
    #[error("manual merge required for task {0}")]
    ManualMerge(String),
    #[error("task is archived; unarchive before syncing")]
    Archived,
}

impl From<IdParseError> for SyncError {
    fn from(e: IdParseError) -> Self {
        Self::BadId(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, SyncError>;
