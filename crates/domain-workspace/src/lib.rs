//! domain-workspace — Workspace aggregate + lifecycle transitions.

use domain_core::{Aggregate, DomainError, Result, Timestamp, WorkspaceId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceName(String);

impl WorkspaceName {
    pub fn new(s: impl Into<String>) -> Result<Self> {
        let s = s.into();
        let trimmed = s.trim();
        if trimmed.is_empty() || trimmed.len() > 128 {
            return Err(DomainError::validation(
                "workspace name must be 1..=128 chars",
            ));
        }
        if !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ' '))
        {
            return Err(DomainError::validation(
                "workspace name may only contain ascii alphanumerics, dash, underscore, space",
            ));
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStatus {
    Created,
    Active,
    Paused,
    Archived,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: WorkspaceName,
    pub description: Option<String>,
    pub status: WorkspaceStatus,
    pub local_only: bool,
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

    #[test]
    fn name_rejects_blank() {
        assert!(WorkspaceName::new("   ").is_err());
    }

    #[test]
    fn name_rejects_funky_chars() {
        assert!(WorkspaceName::new("hi/there").is_err());
    }
}
