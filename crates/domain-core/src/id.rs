use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
#[error("invalid {kind} id: {source}")]
pub struct IdParseError {
    pub kind: &'static str,
    #[source]
    pub source: uuid::Error,
}

macro_rules! define_id {
    ($name:ident, $kind:literal) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self).map_err(|source| IdParseError {
                    kind: $kind,
                    source,
                })
            }
        }
    };
}

define_id!(WorkspaceId, "workspace");
define_id!(RepoId, "repo");
define_id!(TaskId, "task");
define_id!(OutboxEntryId, "outbox-entry");

#[derive(Debug, Error)]
pub enum ProjectIdParseError {
    #[error("project id must not be empty")]
    Empty,
    #[error("project id must start with 'PVT_', got: {0}")]
    InvalidPrefix(String),
}

/// GitHub Projects v2 node ID — the opaque `PVT_…` string the GraphQL API
/// uses to address a project. **Not** a `define_id!` UUID: the canonical
/// value comes from GitHub, so the local row uses the node ID directly as
/// its primary key. The newtype guards against typos by validating the
/// `PVT_` prefix at parse time; the rest is treated as an opaque bag of
/// bytes.
///
/// `#[serde(try_from = "String", into = "String")]` routes deserialization
/// through [`TryFrom<String>`] → [`ProjectId::parse`], so JSON / SQLite /
/// any other on-disk store cannot smuggle an unvalidated prefix into the
/// domain — derived `Deserialize` on a `#[serde(transparent)]` newtype
/// would silently accept any string.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProjectId(String);

impl ProjectId {
    pub fn parse(s: impl Into<String>) -> std::result::Result<Self, ProjectIdParseError> {
        let s = s.into();
        if s.is_empty() {
            return Err(ProjectIdParseError::Empty);
        }
        if !s.starts_with("PVT_") {
            return Err(ProjectIdParseError::InvalidPrefix(s));
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProjectId({})", self.0)
    }
}

impl FromStr for ProjectId {
    type Err = ProjectIdParseError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for ProjectId {
    type Error = ProjectIdParseError;
    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ProjectId> for String {
    fn from(value: ProjectId) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip_via_string() {
        let id = WorkspaceId::new();
        let parsed: WorkspaceId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn ids_of_different_kinds_are_distinct_types() {
        let w = WorkspaceId::from_uuid(Uuid::nil());
        let r = RepoId::from_uuid(Uuid::nil());
        // Same underlying bytes, but distinct types — compile-time guarantee.
        assert_eq!(w.as_uuid(), r.as_uuid());
    }

    #[test]
    fn parse_error_carries_kind() {
        let err = "not-a-uuid".parse::<TaskId>().unwrap_err();
        assert_eq!(err.kind, "task");
    }

    #[test]
    fn serde_serializes_as_bare_string() {
        let id = TaskId::from_uuid(Uuid::nil());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"00000000-0000-0000-0000-000000000000\"");
        let back: TaskId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn project_id_parses_valid_pvt_prefix() {
        let id = ProjectId::parse("PVT_kwHO_abcdef").unwrap();
        assert_eq!(id.as_str(), "PVT_kwHO_abcdef");
    }

    #[test]
    fn project_id_rejects_empty() {
        assert!(matches!(
            ProjectId::parse(""),
            Err(ProjectIdParseError::Empty)
        ));
    }

    #[test]
    fn project_id_rejects_missing_prefix() {
        let err = ProjectId::parse("kwHO_abcdef").unwrap_err();
        assert!(matches!(err, ProjectIdParseError::InvalidPrefix(_)));
    }

    #[test]
    fn project_id_serde_roundtrips_as_bare_string() {
        let id = ProjectId::parse("PVT_kwHO_xyz").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"PVT_kwHO_xyz\"");
        let back: ProjectId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn project_id_deserialize_rejects_missing_prefix() {
        // Without `try_from`, a derived `#[serde(transparent)]` Deserialize
        // would happily wrap any string. We explicitly route through parse
        // so an on-disk corruption or a malformed remote payload cannot
        // smuggle an unvalidated prefix into the domain.
        let err = serde_json::from_str::<ProjectId>("\"not-pvt\"").unwrap_err();
        assert!(err.to_string().contains("PVT_"), "error: {err}");
    }

    #[test]
    fn project_id_deserialize_rejects_empty() {
        let err = serde_json::from_str::<ProjectId>("\"\"").unwrap_err();
        assert!(err.to_string().contains("must not be empty"), "error: {err}");
    }

    #[test]
    fn outbox_entry_id_minted_unique() {
        let a = OutboxEntryId::new();
        let b = OutboxEntryId::new();
        assert_ne!(a, b);
    }
}
