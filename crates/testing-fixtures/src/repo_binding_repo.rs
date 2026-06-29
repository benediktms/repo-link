use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use domain_core::{RepoInstanceId, RepoOriginId, WorkspaceId};
use domain_repo::{RepoBindingView, RepoInstance, RepoOrigin};
use ports::{PortError, PortResult, RepoBindingRepository};

// ---------- Repo binding repository (RFC 0005: origins + instances) -------

#[derive(Default)]
pub struct InMemoryRepoBindingRepository {
    origins: Mutex<HashMap<RepoOriginId, RepoOrigin>>,
    instances: Mutex<HashMap<RepoInstanceId, RepoInstance>>,
}

impl InMemoryRepoBindingRepository {
    pub fn new() -> Self {
        Self::default()
    }

    fn view(&self, instance: RepoInstance) -> PortResult<RepoBindingView> {
        let origins = self.origins.lock().unwrap();
        let origin = origins
            .get(&instance.origin_id)
            .cloned()
            .ok_or_else(|| PortError::NotFound(format!("origin {}", instance.origin_id)))?;
        Ok(RepoBindingView { origin, instance })
    }
}

#[async_trait]
impl RepoBindingRepository for InMemoryRepoBindingRepository {
    async fn save_origin(&self, origin: &RepoOrigin) -> PortResult<()> {
        // Check prefix uniqueness (if non-empty) before inserting
        if !origin.prefix.is_empty() {
            let g = self.origins.lock().unwrap();
            for (id, existing) in g.iter() {
                if existing.prefix == origin.prefix && *id != origin.id {
                    return Err(PortError::Conflict {
                        target: Some("repo_origins.prefix".to_string()),
                        message: format!(
                            "prefix '{}' already taken by origin {}",
                            origin.prefix, id
                        ),
                    });
                }
            }
        }
        self.origins
            .lock()
            .unwrap()
            .insert(origin.id, origin.clone());
        Ok(())
    }

    async fn save_instance(&self, instance: &RepoInstance) -> PortResult<()> {
        self.instances
            .lock()
            .unwrap()
            .insert(instance.id, instance.clone());
        Ok(())
    }

    async fn get(&self, id: RepoInstanceId) -> PortResult<RepoBindingView> {
        let instance = self
            .instances
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| PortError::NotFound(format!("repo instance {id}")))?;
        self.view(instance)
    }

    async fn get_origin(&self, id: RepoOriginId) -> PortResult<RepoOrigin> {
        self.origins
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| PortError::NotFound(format!("repo origin {id}")))
    }

    async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> PortResult<Vec<RepoBindingView>> {
        let instances = self.instances.lock().unwrap();
        let mut rows: Vec<RepoInstance> = instances
            .values()
            .filter(|i| i.workspace_id == workspace_id)
            .cloned()
            .collect();
        rows.sort_by_key(|i| i.created_at);
        drop(instances);
        let mut out = Vec::with_capacity(rows.len());
        for instance in rows {
            out.push(self.view(instance)?);
        }
        Ok(out)
    }

    async fn find_by_canonical_url(
        &self,
        workspace_id: WorkspaceId,
        canonical_url: &str,
    ) -> PortResult<Option<RepoBindingView>> {
        let instances = self.instances.lock().unwrap();
        let instance = instances
            .values()
            .find(|i| i.workspace_id == workspace_id && i.canonical_url == canonical_url)
            .cloned();
        drop(instances);
        match instance {
            Some(i) => Ok(Some(self.view(i)?)),
            None => Ok(None),
        }
    }

    async fn find_origin_by_canonical_url(
        &self,
        canonical_url: &str,
    ) -> PortResult<Option<RepoOrigin>> {
        Ok(self
            .origins
            .lock()
            .unwrap()
            .values()
            .find(|o| o.canonical_url == canonical_url)
            .cloned())
    }

    async fn find_origin_by_prefix(&self, prefix: &str) -> PortResult<Option<RepoOrigin>> {
        if prefix.is_empty() {
            return Ok(None);
        }
        Ok(self
            .origins
            .lock()
            .unwrap()
            .values()
            .find(|o| o.prefix == prefix)
            .cloned())
    }

    async fn find_by_remote_mapping(
        &self,
        _workspace_id: WorkspaceId,
        _provider: &str,
        _remote_id: &str,
    ) -> PortResult<Option<RepoOriginId>> {
        // The in-memory binding repo doesn't model `remote_mappings`
        // (that table is SQLite-specific). Tests that exercise the
        // auto-target's step 2 (`remote_mappings` lookup) need to
        // mock this directly via a custom port impl; the rest of
        // the doctor chain (logical-repo lookup, task save) still
        // works because the in-memory binding repo DOES model the
        // binding table itself.
        Ok(None)
    }

    async fn delete(&self, id: RepoInstanceId) -> PortResult<()> {
        self.instances.lock().unwrap().remove(&id);
        Ok(())
    }
}
