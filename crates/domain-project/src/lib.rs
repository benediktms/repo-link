//! domain-project — Mirror of a GitHub Projects v2 board plus the
//! local-status → project-option mapping. No I/O.
//!
//! Identity is the GitHub node ID itself (`PVT_…`), captured as
//! [`domain_core::ProjectId`] — projects are a 100% mirror of the remote
//! entity, so there is no separate local UUID. Workspaces reference a
//! project via the optional `Workspace.project_id` axis; one project can
//! parent many workspaces.

mod field;
mod issue_type;
mod mapping;
mod project;
mod status;

pub use field::{FieldOption, ProjectField, ProjectFieldKind, assign_field_kinds};
pub use issue_type::{OrgIssueType, OrgIssueTypeRegistry};
pub use mapping::derive_status_mappings;
pub use project::Project;
pub use status::StatusMapping;
