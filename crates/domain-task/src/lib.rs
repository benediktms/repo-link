//! domain-task — Task aggregate, state machine, coordination relations.

use domain_core::{Aggregate, DomainError, RepoId, Result, TaskId, Timestamp, WorkspaceId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Draft,
    Staged,
    Pushed,
    Synced,
    DirtyLocal,
    DirtyRemote,
    Conflict,
    Blocked,
    Archived,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    P0,
    P1,
    P2,
    P3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    BlockedBy,
    Blocks,
    DependsOn,
    Duplicates,
    ParentOf,
    ChildOf,
    RelatedTo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRelation {
    pub kind: RelationKind,
    pub other: TaskId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRef {
    pub provider: String,
    pub remote_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub workspace_id: WorkspaceId,
    pub repo_id: Option<RepoId>,
    pub title: String,
    pub body: String,
    pub state: TaskState,
    pub priority: Priority,
    pub assignees: Vec<String>,
    pub remote: Option<RemoteRef>,
    pub relations: Vec<TaskRelation>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Task {
    pub fn new_draft(
        workspace_id: WorkspaceId,
        repo_id: Option<RepoId>,
        title: String,
    ) -> Result<Self> {
        if title.trim().is_empty() {
            return Err(DomainError::validation("task title is empty"));
        }
        let now = Timestamp::now();
        Ok(Self {
            id: TaskId::new(),
            workspace_id,
            repo_id,
            title,
            body: String::new(),
            state: TaskState::Draft,
            priority: Priority::P3,
            assignees: Vec::new(),
            remote: None,
            relations: Vec::new(),
            created_at: now,
            updated_at: now,
        })
    }

    pub fn stage_for_sync(&mut self) -> Result<()> {
        match self.state {
            TaskState::Draft | TaskState::DirtyLocal => {
                self.state = TaskState::Staged;
                self.touch();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot stage from {other:?}"
            ))),
        }
    }

    pub fn promote_to_remote(&mut self, remote: RemoteRef) -> Result<()> {
        if self.state != TaskState::Staged {
            return Err(DomainError::transition(format!(
                "cannot promote from {:?}",
                self.state
            )));
        }
        self.remote = Some(remote);
        self.state = TaskState::Pushed;
        self.touch();
        Ok(())
    }

    pub fn mark_synced(&mut self) -> Result<()> {
        match self.state {
            TaskState::Pushed | TaskState::DirtyLocal | TaskState::DirtyRemote => {
                self.state = TaskState::Synced;
                self.touch();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot mark synced from {other:?}"
            ))),
        }
    }

    pub fn mark_dirty_local(&mut self) -> Result<()> {
        match self.state {
            TaskState::Synced | TaskState::Pushed => {
                self.state = TaskState::DirtyLocal;
                self.touch();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot mark dirty_local from {other:?}"
            ))),
        }
    }

    pub fn mark_dirty_remote(&mut self) -> Result<()> {
        match self.state {
            TaskState::Synced | TaskState::Pushed => {
                self.state = TaskState::DirtyRemote;
                self.touch();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot mark dirty_remote from {other:?}"
            ))),
        }
    }

    pub fn mark_conflicted(&mut self) -> Result<()> {
        match self.state {
            TaskState::DirtyLocal
            | TaskState::DirtyRemote
            | TaskState::Pushed
            | TaskState::Synced => {
                self.state = TaskState::Conflict;
                self.touch();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot mark conflict from {other:?}"
            ))),
        }
    }

    pub fn mark_blocked(&mut self) {
        self.state = TaskState::Blocked;
        self.touch();
    }

    pub fn archive(&mut self) -> Result<()> {
        if self.state == TaskState::Archived {
            return Err(DomainError::transition("already archived"));
        }
        self.state = TaskState::Archived;
        self.touch();
        Ok(())
    }

    pub fn add_relation(&mut self, kind: RelationKind, other: TaskId) {
        if !self
            .relations
            .iter()
            .any(|r| r.kind == kind && r.other == other)
        {
            self.relations.push(TaskRelation { kind, other });
            self.touch();
        }
    }

    pub fn set_priority(&mut self, priority: Priority) {
        if self.priority != priority {
            self.priority = priority;
            self.touch();
        }
    }

    pub fn set_body(&mut self, body: String) {
        self.body = body;
        self.touch();
    }

    pub fn is_remote_backed(&self) -> bool {
        self.remote.is_some()
    }

    fn touch(&mut self) {
        self.updated_at = Timestamp::now();
    }
}

impl Aggregate for Task {
    type Id = TaskId;

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

    fn draft() -> Task {
        Task::new_draft(WorkspaceId::new(), None, "do the thing".into()).unwrap()
    }

    fn remote_ref() -> RemoteRef {
        RemoteRef {
            provider: "github".into(),
            remote_id: "org/repo#1".into(),
        }
    }

    #[test]
    fn rejects_empty_title() {
        assert!(Task::new_draft(WorkspaceId::new(), None, "  ".into()).is_err());
    }

    #[test]
    fn happy_path_draft_to_synced() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        t.mark_synced().unwrap();
        assert_eq!(t.state, TaskState::Synced);
        assert!(t.is_remote_backed());
    }

    #[test]
    fn promote_requires_staged() {
        let mut t = draft();
        assert!(t.promote_to_remote(remote_ref()).is_err());
    }

    #[test]
    fn dirty_local_then_resync() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        t.mark_synced().unwrap();
        t.mark_dirty_local().unwrap();
        t.stage_for_sync().unwrap();
        assert_eq!(t.state, TaskState::Staged);
    }

    #[test]
    fn relations_are_deduplicated() {
        let mut t = draft();
        let other = TaskId::new();
        t.add_relation(RelationKind::BlockedBy, other);
        t.add_relation(RelationKind::BlockedBy, other);
        assert_eq!(t.relations.len(), 1);
    }

    #[test]
    fn archive_is_terminal() {
        let mut t = draft();
        t.archive().unwrap();
        assert!(t.archive().is_err());
    }
}
