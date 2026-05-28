use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use domain_core::{RepoId, WorkspaceId};
use domain_repo::RepoBinding;
use ports::{PortError, PortResult, RepoBindingRepository};

// ---------- Repo binding repository ---------------------------------------

#[derive(Default)]
pub struct InMemoryRepoBindingRepository {
    inner: Mutex<HashMap<RepoId, RepoBinding>>,
}

impl InMemoryRepoBindingRepository {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl RepoBindingRepository for InMemoryRepoBindingRepository {
    async fn save(&self, binding: &RepoBinding) -> PortResult<()> {
        self.inner
            .lock()
            .unwrap()
            .insert(binding.id, binding.clone());
        Ok(())
    }

    async fn get(&self, id: RepoId) -> PortResult<RepoBinding> {
        self.inner
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| PortError::NotFound(format!("repo {id}")))
    }

    async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> PortResult<Vec<RepoBinding>> {
        let g = self.inner.lock().unwrap();
        let mut rows: Vec<_> = g
            .values()
            .filter(|b| b.workspace_id == workspace_id)
            .cloned()
            .collect();
        rows.sort_by_key(|b| b.created_at);
        Ok(rows)
    }

    async fn find_by_canonical_url(
        &self,
        workspace_id: WorkspaceId,
        canonical_url: &str,
    ) -> PortResult<Option<RepoBinding>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .values()
            .find(|b| b.workspace_id == workspace_id && b.canonical_url == canonical_url)
            .cloned())
    }

    async fn find_by_prefix(&self, prefix: &str) -> PortResult<Option<RepoBinding>> {
        if prefix.is_empty() {
            return Ok(None);
        }
        Ok(self
            .inner
            .lock()
            .unwrap()
            .values()
            .find(|b| b.prefix == prefix)
            .cloned())
    }

    async fn delete(&self, id: RepoId) -> PortResult<()> {
        self.inner.lock().unwrap().remove(&id);
        Ok(())
    }
}
