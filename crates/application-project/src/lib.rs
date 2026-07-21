//! application-project — orchestration for `Project` aggregates and the
//! workspace ↔ project link surface.
//!
//! `rl project link` fetches a project's schema from GitHub over GraphQL and
//! seeds it via [`ProjectService::link_from_snapshot`], which auto-derives the
//! local-status → option mapping (RFC 0001 §3). The remaining operations
//! (`get` / `list` / `map_status` / `map_priority` / `unlink`) are local reads/edits of the
//! mirrored project. [`ProjectService::link`] is a lower-level programmatic
//! seam taking a hand-entered schema; it is not wired to the CLI.

mod dto;
mod error;
mod org_issue_type_service;
mod service;
mod status;

pub use error::{Result, ServiceError};
pub use org_issue_type_service::OrgIssueTypeService;
pub use service::{LinkOutcome, ProjectService};
