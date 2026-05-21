//! domain-repo — Repository binding + worktree links.

use std::path::{Path, PathBuf};

use domain_core::{Aggregate, DomainError, RepoId, Result, Timestamp, WorkspaceId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkStatus {
    /// Path exists and points at the expected repo.
    Linked,
    /// Path exists but hasn't been validated recently.
    Stale,
    /// Path is gone from the filesystem.
    MissingPath,
    /// Operator-detached; kept for audit, not used for routing.
    Detached,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeLink {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub status: LinkStatus,
    pub last_seen_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoBinding {
    pub id: RepoId,
    pub workspace_id: WorkspaceId,
    pub remote_url: String,
    pub canonical_url: String,
    pub tracked_branch: Option<String>,
    pub worktrees: Vec<WorktreeLink>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl RepoBinding {
    pub fn new(workspace_id: WorkspaceId, remote_url: String, canonical_url: String) -> Result<Self> {
        if remote_url.trim().is_empty() {
            return Err(DomainError::validation("remote_url is empty"));
        }
        if canonical_url.trim().is_empty() {
            return Err(DomainError::validation("canonical_url is empty"));
        }
        let now = Timestamp::now();
        Ok(Self {
            id: RepoId::new(),
            workspace_id,
            remote_url,
            canonical_url,
            tracked_branch: None,
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
}
