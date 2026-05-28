//! application-project — orchestration for `Project` aggregates and the
//! workspace ↔ project link surface (RFC 0001 Stage 4).
//!
//! All operations are local-only: project schema is hand-entered through
//! [`ProjectService::link`] and never fetched from GitHub. Stage 5 swaps
//! the GraphQL adapter in behind the same [`LinkProjectCmd`] shape so the
//! service surface doesn't change.

mod dto;
mod error;
mod service;
mod status;

pub use error::{Result, ServiceError};
pub use service::ProjectService;
