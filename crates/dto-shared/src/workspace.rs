use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------- Workspace -----------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDto {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub status: String,
    pub local_only: bool,
    /// Parent GitHub Projects v2 board node ID (`PVT_…`) when the workspace
    /// is linked to one. Omitted from JSON in the projectless case to keep
    /// the existing local-only shape unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// The workspace's default filing repo binding UUID (RFC 0002 D2 step-2).
    /// Workspace config — distinct from the D5-protected per-TASK filing axis
    /// which is never surfaced. Omitted from JSON when unset so existing
    /// workspaces serialise unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filing_repo_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateWorkspaceCmd {
    pub name: String,
    pub description: Option<String>,
    pub local_only: bool,
    /// Optional project to attach the new workspace to. Accepts a project
    /// node ID (`PVT_…`) or `owner/number`; resolution happens in
    /// `WorkspaceService::create` against the local `ProjectRepository`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_spec: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateWorkspaceCmd {
    pub workspace_id: String,
    pub name: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListWorkspacesQuery {
    pub include_archived: bool,
}
