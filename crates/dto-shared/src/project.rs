use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------- Project (RFC 0001) --------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusOptionDto {
    pub option_id: String,
    pub name: String,
    pub ordinal: u32,
    /// The local lifecycle bucket this option is the default for, if any
    /// (`"open"` / `"closed"`). Mirrored from the project's `status_mappings`
    /// collection on serialization so the CLI can show "Backlog → open" in
    /// one view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_for: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusMappingDto {
    pub status: String,
    pub option_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectDto {
    /// `PVT_…` — the GitHub node ID. No separate local UUID.
    pub id: String,
    pub owner_login: String,
    pub number: u64,
    pub title: String,
    pub status_field_id: String,
    pub status_options: Vec<StatusOptionDto>,
    pub status_mappings: Vec<StatusMappingDto>,
    pub archived: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Hand-entered project schema for `rl project link` in Stage 4. Stage 5
/// replaces these flags with a GraphQL fetch — the shape of the payload
/// is the same either way, so the service signature carries over.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkProjectCmd {
    pub node_id: String,
    pub owner_login: String,
    pub number: u64,
    pub title: String,
    pub status_field_id: String,
    pub status_options: Vec<StatusOptionDto>,
    /// Initial mappings the caller wants to seed (e.g. auto-derived from
    /// option-name match). Empty = no defaults set; user configures via
    /// `rl project map`.
    pub initial_mappings: Vec<StatusMappingDto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapStatusCmd {
    /// Project node id (`PVT_…`) or `owner/number` spec.
    pub project_spec: String,
    /// Local lifecycle bucket as a snake-case string (`"open"` / `"closed"`).
    pub status: String,
    pub option_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetWorkspaceProjectCmd {
    pub workspace_id: String,
    /// `None` means detach; `Some(spec)` accepts node id or `owner/number`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_spec: Option<String>,
}
