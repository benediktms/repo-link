use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("not a git repo at {0}")]
    NotARepo(String),
    #[error("git: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, GitError>;
