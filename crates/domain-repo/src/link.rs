//! Small value objects: [`LinkStatus`] and [`WorktreeLink`].

use std::path::PathBuf;

use domain_core::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkStatus {
    /// Path exists and points at the expected repo.
    Linked,
    /// Path exists but hasn't been validated recently.
    Stale,
    /// Path is gone from the filesystem.
    MissingPath,
    /// Operator-detached; kept for audit, not used for routing.
    Detached,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeLink {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub status: LinkStatus,
    pub last_seen_at: Timestamp,
}
