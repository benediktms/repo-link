use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use domain_core::{ProjectId, WorkspaceId};
use domain_project::Project;
use ports::{PortError, PortResult, ProjectRepository};

// ---------- Project repository --------------------------------------------

#[derive(Default)]
pub struct InMemoryProjectRepository {
    inner: Mutex<HashMap<ProjectId, Project>>,
    /// Workspace → project membership map. Mirrors the SQLite world where
    /// `workspaces.project_id` is the join column — by keeping a parallel
    /// index here, `list_by_workspace` doesn't need to scan the workspace
    /// repo, and tests can wire membership directly via `link_workspace`.
    members: Mutex<HashMap<WorkspaceId, ProjectId>>,
}

impl InMemoryProjectRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a workspace to a project for `list_by_workspace` to find. Tests
    /// drive this directly because Stage 3 doesn't ship the
    /// `rl project link` CLI yet — there's no service to call.
    pub fn link_workspace(&self, ws: WorkspaceId, project: ProjectId) {
        self.members.lock().unwrap().insert(ws, project);
    }
}

#[async_trait]
impl ProjectRepository for InMemoryProjectRepository {
    async fn save(&self, project: &Project) -> PortResult<()> {
        self.inner
            .lock()
            .unwrap()
            .insert(project.id.clone(), project.clone());
        Ok(())
    }

    async fn get(&self, id: ProjectId) -> PortResult<Project> {
        self.inner
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| PortError::NotFound(format!("project {id}")))
    }

    async fn list_by_workspace(&self, ws: WorkspaceId) -> PortResult<Vec<Project>> {
        let Some(project_id) = self.members.lock().unwrap().get(&ws).cloned() else {
            return Ok(Vec::new());
        };
        Ok(self
            .inner
            .lock()
            .unwrap()
            .get(&project_id)
            .cloned()
            .into_iter()
            .collect())
    }

    async fn list_all(&self) -> PortResult<Vec<Project>> {
        let mut out: Vec<Project> = self.inner.lock().unwrap().values().cloned().collect();
        out.sort_by(|a, b| {
            a.owner_login
                .cmp(&b.owner_login)
                .then_with(|| a.number.cmp(&b.number))
        });
        Ok(out)
    }

    async fn delete(&self, id: ProjectId) -> PortResult<()> {
        self.inner.lock().unwrap().remove(&id);
        // Mirror the SQL `ON DELETE SET NULL`: any workspace pointing at
        // this project becomes projectless.
        self.members.lock().unwrap().retain(|_, pid| pid != &id);
        Ok(())
    }
}
