use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use domain_core::WorkspaceId;
use domain_workspace::Workspace;
use ports::{PortError, PortResult, WorkspaceRepository};

// ---------- Workspace repository ------------------------------------------

#[derive(Default)]
pub struct InMemoryWorkspaceRepository {
    inner: Mutex<HashMap<WorkspaceId, Workspace>>,
}

impl InMemoryWorkspaceRepository {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl WorkspaceRepository for InMemoryWorkspaceRepository {
    async fn save(&self, workspace: &Workspace) -> PortResult<()> {
        self.inner
            .lock()
            .unwrap()
            .insert(workspace.id, workspace.clone());
        Ok(())
    }

    async fn get(&self, id: WorkspaceId) -> PortResult<Workspace> {
        self.inner
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| PortError::NotFound(format!("workspace {id}")))
    }

    async fn find_by_name(&self, name: &str) -> PortResult<Option<Workspace>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .values()
            .find(|w| w.name.as_str() == name)
            .cloned())
    }

    async fn list(&self, include_archived: bool) -> PortResult<Vec<Workspace>> {
        let g = self.inner.lock().unwrap();
        let mut rows: Vec<_> = g
            .values()
            .filter(|w| {
                include_archived
                    || !matches!(
                        w.status,
                        domain_workspace::WorkspaceStatus::Archived
                            | domain_workspace::WorkspaceStatus::Deleted
                    )
            })
            .cloned()
            .collect();
        rows.sort_by_key(|w| w.created_at);
        Ok(rows)
    }

    async fn delete(&self, id: WorkspaceId) -> PortResult<()> {
        self.inner.lock().unwrap().remove(&id);
        Ok(())
    }
}

#[cfg(test)]
mod smoke {
    use super::*;
    use domain_workspace::{Workspace, WorkspaceName};

    #[tokio::test]
    async fn workspace_save_and_list() {
        let repo = InMemoryWorkspaceRepository::new();
        let w = Workspace::new(WorkspaceName::new("a").unwrap(), None, true);
        repo.save(&w).await.unwrap();
        let listed = repo.list(false).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, w.id);
    }
}
