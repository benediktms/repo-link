//! domain-project — Mirror of a GitHub Projects v2 board plus the
//! local-status → project-option mapping. No I/O.
//!
//! Identity is the GitHub node ID itself (`PVT_…`), captured as
//! [`domain_core::ProjectId`] — projects are a 100% mirror of the remote
//! entity, so there is no separate local UUID. Workspaces reference a
//! project via the optional `Workspace.project_id` axis; one project can
//! parent many workspaces.

mod project;
mod status;

pub use project::Project;
pub use status::{StatusMapping, StatusOption};
