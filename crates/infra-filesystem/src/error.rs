use thiserror::Error;

#[derive(Debug, Error)]
pub enum FsError {
    #[error("notify: {0}")]
    Notify(#[from] notify::Error),
}
