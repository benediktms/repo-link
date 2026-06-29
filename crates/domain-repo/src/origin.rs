//! [`RepoOrigin`] aggregate — the shared identity of a repo, keyed on
//! `canonical_url`. Owns everything intrinsic to "the same code on disk":
//! `prefix`, `name`, `aliases`, `remote_url`. One origin per canonical URL
//! across all workspaces (RFC 0005 §D1). Workspace membership — and the
//! worktrees checked out for it — live on [`crate::RepoInstance`].

use domain_core::{Aggregate, DomainError, RepoOriginId, Result, Timestamp};
use serde::{Deserialize, Serialize};

use crate::naming::{derive_name, derive_prefix, is_valid_prefix};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoOrigin {
    pub id: RepoOriginId,
    pub canonical_url: String,
    pub remote_url: String,
    /// Human-friendly handle. Defaults to the canonical URL's last
    /// segment; editable via [`Self::set_name`]. Identity stays on
    /// `canonical_url` — name is an affordance, not a key.
    pub name: String,
    /// Alternative handles for this repo. Order is preserved on disk;
    /// lookups are exact-match (not substring). An alias may not collide
    /// with the current `name`. Repo-global (RFC 0005): shared across every
    /// workspace that has an instance of this origin.
    pub aliases: Vec<String>,
    /// Short globally-unique handle used to assemble friendly task IDs
    /// (`prefix-hash`, e.g. `rlk-ak7`). Derived from `name` via
    /// [`crate::derive_prefix`] at attach time, with the persistence layer
    /// breaking duplicates by appending `1`/`2`/… until unique. Sticky
    /// once set — renaming the repo does not re-derive the prefix.
    /// Empty string is the "not yet set" sentinel pre-backfill.
    pub prefix: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl RepoOrigin {
    pub fn new(remote_url: String, canonical_url: String) -> Result<Self> {
        if remote_url.trim().is_empty() {
            return Err(DomainError::validation("remote_url is empty"));
        }
        if canonical_url.trim().is_empty() {
            return Err(DomainError::validation("canonical_url is empty"));
        }
        let name = derive_name(&canonical_url);
        if name.trim().is_empty() {
            return Err(DomainError::validation(
                "could not derive a non-empty name from canonical_url",
            ));
        }
        let now = Timestamp::now();
        let prefix = derive_prefix(&name);
        Ok(Self {
            id: RepoOriginId::new(),
            canonical_url,
            remote_url,
            name,
            aliases: Vec::new(),
            prefix,
            created_at: now,
            updated_at: now,
        })
    }

    /// Replace the prefix wholesale. Intended for the persistence layer
    /// to apply collision-breaking suffixes (e.g. `pck` → `pck1`) and
    /// for the `rl repo set-prefix` / `repo attach --prefix` override.
    /// Validates against `^[a-z][a-z0-9]{1,19}$` to keep the composite
    /// ID human-typeable.
    pub fn set_prefix(&mut self, new_prefix: String) -> Result<()> {
        if !is_valid_prefix(&new_prefix) {
            return Err(DomainError::validation(
                "prefix must match ^[a-z][a-z0-9]{1,19}$ (2-20 lowercase alnum, must start with a letter)",
            ));
        }
        if self.prefix == new_prefix {
            return Ok(());
        }
        self.prefix = new_prefix;
        self.touch();
        Ok(())
    }

    /// Set a new short name. Trims whitespace, rejects an empty result,
    /// and rejects a name that would collide with an existing alias on
    /// this origin (to keep the name/alias union unambiguous).
    pub fn set_name(&mut self, new_name: String) -> Result<()> {
        let trimmed = new_name.trim();
        if trimmed.is_empty() {
            return Err(DomainError::validation("name is empty"));
        }
        if trimmed.parse::<RepoOriginId>().is_ok() {
            return Err(DomainError::validation(
                "name may not be a UUID — that namespace is reserved for ID-based resolution",
            ));
        }
        if self.aliases.iter().any(|a| a == trimmed) {
            return Err(DomainError::validation(
                "name would collide with an existing alias",
            ));
        }
        if self.name == trimmed {
            return Ok(()); // idempotent no-op
        }
        self.name = trimmed.to_string();
        self.touch();
        Ok(())
    }

    /// Add an alias. Trims whitespace, rejects an empty result, rejects
    /// an alias equal to the current `name` (would mask the name), and
    /// deduplicates against existing aliases. Returns `true` if the
    /// alias was added, `false` if it was already present.
    pub fn add_alias(&mut self, alias: String) -> Result<bool> {
        let trimmed = alias.trim();
        if trimmed.is_empty() {
            return Err(DomainError::validation("alias is empty"));
        }
        if trimmed.parse::<RepoOriginId>().is_ok() {
            return Err(DomainError::validation(
                "alias may not be a UUID — that namespace is reserved for ID-based resolution",
            ));
        }
        if trimmed == self.name {
            return Err(DomainError::validation(
                "alias would collide with the current name",
            ));
        }
        if self.aliases.iter().any(|a| a == trimmed) {
            return Ok(false);
        }
        self.aliases.push(trimmed.to_string());
        self.touch();
        Ok(true)
    }

    /// Remove an alias by exact match. Returns `true` if removed,
    /// `false` if no such alias existed.
    pub fn remove_alias(&mut self, alias: &str) -> bool {
        let before = self.aliases.len();
        self.aliases.retain(|a| a != alias);
        let removed = self.aliases.len() != before;
        if removed {
            self.touch();
        }
        removed
    }

    fn touch(&mut self) {
        self.updated_at = Timestamp::now();
    }
}

impl Aggregate for RepoOrigin {
    type Id = RepoOriginId;

    fn id(&self) -> Self::Id {
        self.id
    }

    fn created_at(&self) -> Timestamp {
        self.created_at
    }

    fn updated_at(&self) -> Timestamp {
        self.updated_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> RepoOrigin {
        RepoOrigin::new(
            "git@github.com:org/repo.git".into(),
            "github.com/org/repo".into(),
        )
        .unwrap()
    }

    #[test]
    fn rejects_empty_remote() {
        let err = RepoOrigin::new("  ".into(), "x".into()).unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn new_origin_derives_name_from_canonical() {
        let o = origin();
        assert_eq!(o.name, "repo");
        assert!(o.aliases.is_empty());
    }

    #[test]
    fn set_name_rejects_empty() {
        let mut o = origin();
        assert!(o.set_name("   ".into()).is_err());
        assert_eq!(o.name, "repo"); // unchanged
    }

    #[test]
    fn set_name_rejects_alias_collision() {
        let mut o = origin();
        o.add_alias("gateway".into()).unwrap();
        let err = o.set_name("gateway".into()).unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn set_name_idempotent_no_op() {
        let mut o = origin();
        let before = o.updated_at;
        o.set_name("repo".into()).unwrap();
        assert_eq!(o.updated_at, before);
    }

    #[test]
    fn add_alias_dedupes() {
        let mut o = origin();
        assert!(o.add_alias("gateway".into()).unwrap());
        assert!(!o.add_alias("gateway".into()).unwrap()); // idempotent
        assert_eq!(o.aliases, vec!["gateway".to_string()]);
    }

    #[test]
    fn add_alias_trims_and_rejects_empty() {
        let mut o = origin();
        assert!(o.add_alias("  gw  ".into()).unwrap());
        assert_eq!(o.aliases, vec!["gw".to_string()]);
        assert!(o.add_alias("   ".into()).is_err());
    }

    #[test]
    fn add_alias_rejects_collision_with_name() {
        let mut o = origin();
        let err = o.add_alias("repo".into()).unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn remove_alias_returns_false_when_absent() {
        let mut o = origin();
        assert!(!o.remove_alias("not-there"));
        o.add_alias("gw".into()).unwrap();
        assert!(o.remove_alias("gw"));
        assert!(o.aliases.is_empty());
    }

    // UUID-shaped strings are reserved for the UUID resolution path on the
    // application side; letting them through as names/aliases would make
    // some handles unreachable.
    #[test]
    fn set_name_rejects_uuid_shaped_value() {
        let mut o = origin();
        let err = o
            .set_name("c08c09c5-4ac2-4a43-96ea-d574a580fde5".into())
            .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn add_alias_rejects_uuid_shaped_value() {
        let mut o = origin();
        let err = o
            .add_alias("c08c09c5-4ac2-4a43-96ea-d574a580fde5".into())
            .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    // ---- Friendly task ID prefix derivation -----------------------------

    #[test]
    fn new_origin_derives_prefix_from_name() {
        let o = origin();
        // Single-word "repo" → 'r' + consonant 'p' + fallback alphabetic 'e'.
        assert_eq!(o.name, "repo");
        assert_eq!(o.prefix, "rpe");
    }

    #[test]
    fn set_prefix_rejects_invalid_and_keeps_old() {
        let mut o = origin();
        let before = o.prefix.clone();
        assert!(o.set_prefix("RLK".into()).is_err());
        assert_eq!(o.prefix, before);
    }

    #[test]
    fn set_prefix_applies_valid_value_and_touches() {
        let mut o = origin();
        let before = o.updated_at;
        o.set_prefix("xyz".into()).unwrap();
        assert_eq!(o.prefix, "xyz");
        assert!(o.updated_at >= before);
    }

    #[test]
    fn set_prefix_idempotent_no_op() {
        let mut o = origin();
        let current = o.prefix.clone();
        let before = o.updated_at;
        o.set_prefix(current).unwrap();
        assert_eq!(o.updated_at, before);
    }
}
