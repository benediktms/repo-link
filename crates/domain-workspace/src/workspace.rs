//! `Workspace` aggregate + lifecycle transitions.

use crate::{WorkspaceName, WorkspaceStatus};
use domain_core::{Aggregate, DomainError, ProjectId, RepoId, Result, Timestamp, WorkspaceId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: WorkspaceName,
    pub description: Option<String>,
    pub status: WorkspaceStatus,
    pub local_only: bool,
    /// Optional parent GitHub Projects v2 board. When `Some`, the project
    /// is the primary sync target for tasks in this workspace (per RFC
    /// 0001 §3 D1). `None` = the local-only / projectless path; existing
    /// workspaces stay valid because the field is purely additive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// The workspace's **default filing repo** (RFC 0002): where a task's
    /// backing GitHub issue is filed when nothing more specific applies. New
    /// and additive — supersedes RFC 0001's deferred `creation_default_repo_id`.
    /// `None` means "no default", and the D2 resolution chain falls through to
    /// the task's logical `repo_id`, so behaviour is unchanged. Surfaced on
    /// `WorkspaceDto` as workspace config (set via `rl workspace set-filing-repo`
    /// per RFC 0002 §4, GitHub #121); distinct from the D5-protected per-TASK
    /// filing axis which is never surfaced on the task boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filing_repo_id: Option<RepoId>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Workspace {
    pub fn new(name: WorkspaceName, description: Option<String>, local_only: bool) -> Self {
        let now = Timestamp::now();
        Self {
            id: WorkspaceId::new(),
            name,
            description,
            status: WorkspaceStatus::Created,
            local_only,
            project_id: None,
            filing_repo_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn activate(&mut self) -> Result<()> {
        match self.status {
            WorkspaceStatus::Created | WorkspaceStatus::Paused => {
                self.status = WorkspaceStatus::Active;
                self.touch();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot activate from {other:?}"
            ))),
        }
    }

    pub fn pause(&mut self) -> Result<()> {
        if self.status == WorkspaceStatus::Active {
            self.status = WorkspaceStatus::Paused;
            self.touch();
            Ok(())
        } else {
            Err(DomainError::transition(format!(
                "cannot pause from {:?}",
                self.status
            )))
        }
    }

    pub fn archive(&mut self) -> Result<()> {
        match self.status {
            WorkspaceStatus::Created | WorkspaceStatus::Active | WorkspaceStatus::Paused => {
                self.status = WorkspaceStatus::Archived;
                self.touch();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot archive from {other:?}"
            ))),
        }
    }

    fn touch(&mut self) {
        self.updated_at = Timestamp::now();
    }
}

impl Aggregate for Workspace {
    type Id = WorkspaceId;

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

    fn ws() -> Workspace {
        Workspace::new(WorkspaceName::new("scratch").unwrap(), None, true)
    }

    #[test]
    fn activate_from_created() {
        let mut w = ws();
        w.activate().unwrap();
        assert_eq!(w.status, WorkspaceStatus::Active);
    }

    #[test]
    fn cannot_activate_archived() {
        let mut w = ws();
        w.archive().unwrap();
        assert!(w.activate().is_err());
    }

    #[test]
    fn pause_requires_active() {
        let mut w = ws();
        assert!(w.pause().is_err());
        w.activate().unwrap();
        w.pause().unwrap();
        assert_eq!(w.status, WorkspaceStatus::Paused);
    }
}
