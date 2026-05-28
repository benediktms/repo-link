//! application-sync — orchestrates remote promotion / push / pull.
//!
//! Local SQLite is authoritative for draft state; once a task has been
//! pushed, GitHub becomes the source of truth. Sync transitions follow
//! [`SyncState`]; lifecycle ([`TaskStatus`]) is orthogonal and only
//! consulted to skip Archived tasks.

mod error;
mod service;
mod summary;

pub use error::{Result, SyncError};
pub use service::SyncService;
