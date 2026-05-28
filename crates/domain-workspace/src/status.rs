//! Workspace lifecycle status.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStatus {
    Created,
    Active,
    Paused,
    Archived,
    Deleted,
}
