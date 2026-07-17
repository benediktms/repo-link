//! Org-scoped native issue-type registry (RFC 0006 D5/D8).
//!
//! GitHub's native "issue type" is an *organization-level* catalog, not a
//! per-board field — one set of types is shared across every repo/project the
//! org owns. This module models that catalog as a cache decoupled from any
//! [`crate::Project`]: the registry is keyed on the owner login and holds the
//! `(issue_type_id, name)` pairs the org defines.
//!
//! Availability (D8) is a property of the registry itself: a user-owned owner
//! (personal account) or an org with the feature disabled has *no* types, and
//! that empty set must read as "unavailable" rather than an error anywhere it
//! is fetched or loaded.
//!
//! The type is deliberately named [`OrgIssueType`] (a fetched registry entry)
//! to leave the plain `IssueType` name free for the future domain-task local
//! enum (RFC 0006 D7, tracked by the type-on-tasks follow-up, #228). No
//! resolve-by-name helper lives here — that is the follow-up's concern.

use serde::{Deserialize, Serialize};

/// One native issue type as defined at the organization level: the stable
/// per-org `issue_type_id` plus its display `name`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgIssueType {
    pub issue_type_id: String,
    pub name: String,
}

/// The full native issue-type catalog for one repository owner (org). A cache,
/// not a source of truth: it is (re)fetched at `rl project link` time and
/// replaced wholesale. An empty `types` set is the D8 "type unavailable for
/// this org" signal — a user-owned owner or a disabled feature both produce
/// it, and neither is an error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgIssueTypeRegistry {
    pub owner_login: String,
    pub types: Vec<OrgIssueType>,
}

impl OrgIssueTypeRegistry {
    pub fn new(owner_login: impl Into<String>, types: Vec<OrgIssueType>) -> Self {
        Self {
            owner_login: owner_login.into(),
            types,
        }
    }

    /// The D8 availability signal: native issue types are available for this
    /// org iff the registry has at least one type. An absent/empty registry
    /// (user-owned owner, feature disabled) reports `false` without erroring.
    pub fn is_available(&self) -> bool {
        !self.types.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(id: &str, name: &str) -> OrgIssueType {
        OrgIssueType {
            issue_type_id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn registry_is_available_when_types_present() {
        let reg = OrgIssueTypeRegistry::new("acme", vec![ty("IT_1", "Bug"), ty("IT_2", "Feature")]);
        assert!(reg.is_available());
    }

    #[test]
    fn registry_unavailable_when_empty() {
        // A user-owned owner or a feature-disabled org yields an empty set:
        // unavailable, but NOT an error (D8).
        let reg = OrgIssueTypeRegistry::new("some-user", vec![]);
        assert!(!reg.is_available());
    }
}
