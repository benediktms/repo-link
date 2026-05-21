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
                Uuid::parse_str(s)
                    .map(Self)
                    .map_err(|source| IdParseError { kind: $kind, source })
            }
        }
    };
}

define_id!(WorkspaceId, "workspace");
define_id!(RepoId, "repo");
define_id!(TaskId, "task");

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
}
