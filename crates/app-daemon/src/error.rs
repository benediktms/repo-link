use thiserror::Error;

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error(transparent)]
    Port(#[from] ports::PortError),
    #[error("workspace service: {0}")]
    Workspace(String),
    #[error("binding service: {0}")]
    Binding(String),
    #[error("sync: {0}")]
    Sync(String),
}
