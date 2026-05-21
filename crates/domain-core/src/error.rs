use thiserror::Error;

use crate::IdParseError;

pub type Result<T, E = DomainError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum DomainError {
    #[error("invalid state transition: {0}")]
    InvalidTransition(String),

    #[error("validation failed: {0}")]
    Validation(String),

    #[error(transparent)]
    Id(#[from] IdParseError),
}

impl DomainError {
    pub fn transition(msg: impl Into<String>) -> Self {
        Self::InvalidTransition(msg.into())
    }

    pub fn validation(msg: impl Into<String>) -> Self {
        Self::Validation(msg.into())
    }
}
