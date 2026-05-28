//! domain-workspace — Workspace aggregate + lifecycle transitions.

mod name;
mod status;
mod workspace;

pub use name::WorkspaceName;
pub use status::WorkspaceStatus;
pub use workspace::Workspace;
