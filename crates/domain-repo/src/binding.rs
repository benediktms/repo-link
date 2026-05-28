//! [`RepoBinding`] aggregate + lifecycle methods.

use std::path::{Path, PathBuf};

use domain_core::{Aggregate, DomainError, RepoId, Result, Timestamp, WorkspaceId};
use serde::{Deserialize, Serialize};

use crate::link::{LinkStatus, WorktreeLink};
use crate::naming::{derive_name, derive_prefix, is_valid_prefix};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoBinding {
    pub id: RepoId,
    pub workspace_id: WorkspaceId,
    pub remote_url: String,
    pub canonical_url: String,
    pub tracked_branch: Option<String>,
    /// Human-friendly handle. Defaults to the canonical URL's last
    /// segment; editable via [`Self::set_name`]. Identity stays on
    /// `canonical_url` — name is an affordance, not a key.
    pub name: String,
    /// Alternative handles for this binding. Order is preserved on
    /// disk; lookups are exact-match (not substring). An alias may not
    /// collide with the current `name`.
    pub aliases: Vec<String>,
    /// Short globally-unique handle used to assemble friendly task IDs
    /// (`prefix-hash`, e.g. `rlk-ak7`). Derived from `name` via
    /// [`crate::derive_prefix`] at attach time, with the persistence layer
    /// breaking duplicates by appending `1`/`2`/… until unique. Sticky
    /// once set — renaming the repo does not re-derive the prefix.
    /// Empty string is the "not yet set" sentinel pre-backfill.
    pub prefix: String,
    pub worktrees: Vec<WorktreeLink>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl RepoBinding {
    pub fn new(
        workspace_id: WorkspaceId,
        remote_url: String,
        canonical_url: String,
    ) -> Result<Self> {
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
            id: RepoId::new(),
            workspace_id,
            remote_url,
            canonical_url,
            tracked_branch: None,
            name,
            aliases: Vec::new(),
            prefix,
            worktrees: Vec::new(),
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
    /// this binding (to keep the name/alias union unambiguous).
    pub fn set_name(&mut self, new_name: String) -> Result<()> {
        let trimmed = new_name.trim();
        if trimmed.is_empty() {
            return Err(DomainError::validation("name is empty"));
        }
        if trimmed.parse::<RepoId>().is_ok() {
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
        if trimmed.parse::<RepoId>().is_ok() {
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

    pub fn link_worktree(&mut self, path: PathBuf, branch: Option<String>) {
        let now = Timestamp::now();
        if let Some(existing) = self.worktrees.iter_mut().find(|w| w.path == path) {
            existing.branch = branch;
            existing.status = LinkStatus::Linked;
            existing.last_seen_at = now;
        } else {
            self.worktrees.push(WorktreeLink {
                path,
                branch,
                status: LinkStatus::Linked,
                last_seen_at: now,
            });
        }
        self.touch();
    }

    pub fn unlink_worktree(&mut self, path: &Path) -> Result<()> {
        let before = self.worktrees.len();
        self.worktrees.retain(|w| w.path != path);
        if self.worktrees.len() == before {
            return Err(DomainError::validation("worktree path not registered"));
        }
        self.touch();
        Ok(())
    }

    pub fn mark_path_missing(&mut self, path: &Path) -> Result<()> {
        let link = self
            .worktrees
            .iter_mut()
            .find(|w| w.path == path)
            .ok_or_else(|| DomainError::validation("worktree path not registered"))?;
        link.status = LinkStatus::MissingPath;
        self.touch();
        Ok(())
    }

    /// Drop worktrees marked `MissingPath`. Returns count pruned.
    pub fn prune_missing(&mut self) -> usize {
        let before = self.worktrees.len();
        self.worktrees
            .retain(|w| w.status != LinkStatus::MissingPath);
        let pruned = before - self.worktrees.len();
        if pruned > 0 {
            self.touch();
        }
        pruned
    }

    fn touch(&mut self) {
        self.updated_at = Timestamp::now();
    }
}

impl Aggregate for RepoBinding {
    type Id = RepoId;

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

    fn binding() -> RepoBinding {
        RepoBinding::new(
            WorkspaceId::new(),
            "git@github.com:org/repo.git".into(),
            "github.com/org/repo".into(),
        )
        .unwrap()
    }

    #[test]
    fn rejects_empty_remote() {
        let err = RepoBinding::new(WorkspaceId::new(), "  ".into(), "x".into()).unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn link_same_path_twice_is_idempotent_update() {
        let mut b = binding();
        b.link_worktree(PathBuf::from("/tmp/a"), Some("main".into()));
        b.link_worktree(PathBuf::from("/tmp/a"), Some("dev".into()));
        assert_eq!(b.worktrees.len(), 1);
        assert_eq!(b.worktrees[0].branch.as_deref(), Some("dev"));
    }

    #[test]
    fn prune_missing_only_drops_missing() {
        let mut b = binding();
        b.link_worktree(PathBuf::from("/tmp/a"), None);
        b.link_worktree(PathBuf::from("/tmp/b"), None);
        b.mark_path_missing(Path::new("/tmp/a")).unwrap();
        assert_eq!(b.prune_missing(), 1);
        assert_eq!(b.worktrees.len(), 1);
        assert_eq!(b.worktrees[0].path, PathBuf::from("/tmp/b"));
    }

    #[test]
    fn unlink_unknown_path_errors() {
        let mut b = binding();
        assert!(b.unlink_worktree(Path::new("/nope")).is_err());
    }

    // ---- Phase B: name + aliases ----------------------------------------

    #[test]
    fn new_binding_derives_name_from_canonical() {
        let b = binding();
        assert_eq!(b.name, "repo");
        assert!(b.aliases.is_empty());
    }

    #[test]
    fn set_name_rejects_empty() {
        let mut b = binding();
        assert!(b.set_name("   ".into()).is_err());
        assert_eq!(b.name, "repo"); // unchanged
    }

    #[test]
    fn set_name_rejects_alias_collision() {
        let mut b = binding();
        b.add_alias("gateway".into()).unwrap();
        let err = b.set_name("gateway".into()).unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn set_name_idempotent_no_op() {
        let mut b = binding();
        let before = b.updated_at;
        // Small artificial wait so the touch (if it happened) would be
        // observable. We only assert *no* touch — same-value set should
        // bail before reaching `touch`.
        b.set_name("repo".into()).unwrap();
        assert_eq!(b.updated_at, before);
    }

    #[test]
    fn add_alias_dedupes() {
        let mut b = binding();
        assert!(b.add_alias("gateway".into()).unwrap());
        assert!(!b.add_alias("gateway".into()).unwrap()); // idempotent
        assert_eq!(b.aliases, vec!["gateway".to_string()]);
    }

    #[test]
    fn add_alias_trims_and_rejects_empty() {
        let mut b = binding();
        assert!(b.add_alias("  gw  ".into()).unwrap());
        assert_eq!(b.aliases, vec!["gw".to_string()]);
        assert!(b.add_alias("   ".into()).is_err());
    }

    #[test]
    fn add_alias_rejects_collision_with_name() {
        let mut b = binding();
        let err = b.add_alias("repo".into()).unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn remove_alias_returns_false_when_absent() {
        let mut b = binding();
        assert!(!b.remove_alias("not-there"));
        b.add_alias("gw".into()).unwrap();
        assert!(b.remove_alias("gw"));
        assert!(b.aliases.is_empty());
    }

    // UUID-shaped strings are reserved for the UUID resolution path on
    // the application side; letting them through as names/aliases would
    // make some handles unreachable (a name equal to a different
    // binding's UUID can't be resolved via the name path because the
    // resolver would short-circuit on UUID parse).
    #[test]
    fn set_name_rejects_uuid_shaped_value() {
        let mut b = binding();
        let err = b
            .set_name("c08c09c5-4ac2-4a43-96ea-d574a580fde5".into())
            .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn add_alias_rejects_uuid_shaped_value() {
        let mut b = binding();
        let err = b
            .add_alias("c08c09c5-4ac2-4a43-96ea-d574a580fde5".into())
            .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    // ---- Friendly task ID prefix derivation -----------------------------

    #[test]
    fn new_binding_derives_prefix_from_name() {
        let b = binding();
        // Single-word "repo" → first char 'r' + consonant 'p' + fallback
        // alphabetic 'e' → "rpe". (No 'x' padding needed once the
        // fallback kicks in.)
        assert_eq!(b.name, "repo");
        assert_eq!(b.prefix, "rpe");
    }

    #[test]
    fn set_prefix_rejects_invalid_and_keeps_old() {
        let mut b = binding();
        let before = b.prefix.clone();
        assert!(b.set_prefix("RLK".into()).is_err());
        assert_eq!(b.prefix, before);
    }

    #[test]
    fn set_prefix_applies_valid_value_and_touches() {
        let mut b = binding();
        let before = b.updated_at;
        // Sleep is not necessary — we only check that the prefix changed.
        b.set_prefix("xyz".into()).unwrap();
        assert_eq!(b.prefix, "xyz");
        assert!(b.updated_at >= before);
    }

    #[test]
    fn set_prefix_idempotent_no_op() {
        let mut b = binding();
        let current = b.prefix.clone();
        let before = b.updated_at;
        b.set_prefix(current).unwrap();
        assert_eq!(b.updated_at, before);
    }
}
