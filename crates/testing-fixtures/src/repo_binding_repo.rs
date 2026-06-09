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

    async fn find_by_remote_mapping(
        &self,
        _provider: &str,
        _remote_id: &str,
    ) -> PortResult<Option<RepoId>> {
        // The in-memory binding repo doesn't model `remote_mappings`
        // (that table is SQLite-specific). Tests that exercise the
        // auto-target's step 2 (`remote_mappings` lookup) need to mock
        // this directly via a custom port impl; the rest of the doctor
        // chain (logical-repo lookup, task save) still works because
        // the in-memory binding repo DOES model the binding table
        // itself.
        Ok(None)
    }

    async fn delete(&self, id: RepoId) -> PortResult<()> {
        self.inner.lock().unwrap().remove(&id);
        Ok(())
    }
}
