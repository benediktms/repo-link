use serde::{Deserialize, Serialize};

use crate::RemoteRefDto;

// ---------- Sync ----------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromoteTaskCmd {
    pub task_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushTaskCmd {
    pub task_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullTaskCmd {
    pub task_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncSummaryDto {
    pub task_id: String,
    pub previous_state: String,
    pub new_state: String,
    pub decision: String,
    pub remote: Option<RemoteRefDto>,
    /// Free-text caveat the CLI surfaces alongside a successful sync verb,
    /// when the operation completed but the user should know about an
    /// anomaly (e.g. linking to a URL whose live issue has been transferred
    /// elsewhere). `None` on the happy path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}
