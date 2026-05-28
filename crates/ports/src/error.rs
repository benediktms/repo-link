//! Port-level error type.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PortError {
    #[error("not found: {0}")]
    NotFound(String),

    /// A uniqueness violation. `target` names the logical constraint the
    /// backend reported (e.g. `"tasks.hash"`, `"repos.prefix"`) when it
    /// can — adapters translate their native error into this structured
    /// form so the application layer can drive retry logic off the
    /// target instead of substring-matching backend-specific message
    /// text. `None` when the backend gives no usable target.
    #[error("conflict{}: {message}", .target.as_deref().map(|t| format!(" on {t}")).unwrap_or_default())]
    Conflict {
        target: Option<String>,
        message: String,
    },

    #[error("backend failure: {0}")]
    Backend(String),

    #[error("network failure: {0}")]
    Network(String),

    /// The remote issue at `from_canonical#from_remote_id` was administratively
    /// transferred to `to_canonical#to_remote_id` (GitHub returned 301 with a
    /// `Location` header). Adapters surface this *typed* error instead of a
    /// raw network failure so callers can offer a verified re-link rather than
    /// asking the user to diagnose an opaque HTTP code.
    #[error(
        "remote issue {from_canonical}#{from_remote_id} moved to {to_canonical}#{to_remote_id}"
    )]
    IssueMoved {
        from_canonical: String,
        from_remote_id: String,
        to_canonical: String,
        to_remote_id: String,
    },
}

impl PortError {
    /// The logical target of a uniqueness [`PortError::Conflict`]
    /// (e.g. `"tasks.hash"`, `"repos.prefix"`), if the backend reported
    /// one. Returns `None` for non-conflict errors or conflicts without
    /// a target.
    pub fn conflict_target(&self) -> Option<&str> {
        match self {
            PortError::Conflict {
                target: Some(t), ..
            } => Some(t.as_str()),
            _ => None,
        }
    }
}

pub type PortResult<T> = std::result::Result<T, PortError>;
