//! [`RepoInstance`] aggregate — a workspace's membership of a
//! [`crate::RepoOrigin`]: the `(workspace, origin)` pair plus per-workspace
//! state (`tracked_branch`) and the worktrees checked out for it. This is the
//! renamed pre-RFC-0005 `repos` row (RFC 0005 §D1); the shared identity
//! (`prefix`/`name`/`aliases`/`remote_url`) lives on the origin.

use std::path::{Path, PathBuf};

use domain_core::{
    Aggregate, DomainError, RepoInstanceId, RepoOriginId, Result, Timestamp, WorkspaceId,
};
use serde::{Deserialize, Serialize};

use crate::link::{LinkStatus, WorktreeLink};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoInstance {
    pub id: RepoInstanceId,
    pub workspace_id: WorkspaceId,
    pub origin_id: RepoOriginId,
    pub tracked_branch: Option<String>,
    /// Denormalized copy of the origin's `canonical_url`. Backs the
    /// `UNIQUE(workspace_id, canonical_url)` membership guard and is a
    /// convenient join/debug key (RFC 0005 §D1). Kept in sync with the
    /// origin at the persistence/attach layer; the domain does not enforce
    /// cross-aggregate equality.
    pub canonical_url: String,
    pub worktrees: Vec<WorktreeLink>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl RepoInstance {
    pub fn new(
        workspace_id: WorkspaceId,
        origin_id: RepoOriginId,
        canonical_url: String,
        tracked_branch: Option<String>,
    ) -> Result<Self> {
        if canonical_url.trim().is_empty() {
            return Err(DomainError::validation("canonical_url is empty"));
        }
        let now = Timestamp::now();
        Ok(Self {
            id: RepoInstanceId::new(),
            workspace_id,
            origin_id,
            tracked_branch,
            canonical_url,
            worktrees: Vec::new(),
            created_at: now,
            updated_at: now,
        })
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

impl Aggregate for RepoInstance {
    type Id = RepoInstanceId;

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

    fn instance() -> RepoInstance {
        RepoInstance::new(
            WorkspaceId::new(),
            RepoOriginId::new(),
            "github.com/org/repo".into(),
            None,
        )
        .unwrap()
    }

    #[test]
    fn rejects_empty_canonical() {
        let err =
            RepoInstance::new(WorkspaceId::new(), RepoOriginId::new(), "  ".into(), None).unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn link_same_path_twice_is_idempotent_update() {
        let mut i = instance();
        i.link_worktree(PathBuf::from("/tmp/a"), Some("main".into()));
        i.link_worktree(PathBuf::from("/tmp/a"), Some("dev".into()));
        assert_eq!(i.worktrees.len(), 1);
        assert_eq!(i.worktrees[0].branch.as_deref(), Some("dev"));
    }

    #[test]
    fn prune_missing_only_drops_missing() {
        let mut i = instance();
        i.link_worktree(PathBuf::from("/tmp/a"), None);
        i.link_worktree(PathBuf::from("/tmp/b"), None);
        i.mark_path_missing(Path::new("/tmp/a")).unwrap();
        assert_eq!(i.prune_missing(), 1);
        assert_eq!(i.worktrees.len(), 1);
        assert_eq!(i.worktrees[0].path, PathBuf::from("/tmp/b"));
    }

    #[test]
    fn unlink_unknown_path_errors() {
        let mut i = instance();
        assert!(i.unlink_worktree(Path::new("/nope")).is_err());
    }
}
