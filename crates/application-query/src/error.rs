use domain_core::IdParseError;
use ports::PortError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum QueryError {
    #[error(transparent)]
    Port(#[from] PortError),
    #[error("invalid id: {0}")]
    BadId(String),
}

impl From<IdParseError> for QueryError {
    fn from(e: IdParseError) -> Self {
        Self::BadId(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, QueryError>;
