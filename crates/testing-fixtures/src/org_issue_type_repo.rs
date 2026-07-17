//! In-memory [`OrgIssueTypeRepository`] stub (RFC 0006 D5) for the
//! application-project service tests. Backed by a `Mutex<HashMap>` keyed on
//! owner login; `save` replaces an owner's registry, `get` returns the stored
//! one or an empty registry for an unknown owner (the D8 no-error contract).

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use domain_project::OrgIssueTypeRegistry;
use ports::{OrgIssueTypeRepository, PortResult};

#[derive(Default)]
pub struct InMemoryOrgIssueTypeRepository {
    by_owner: Mutex<HashMap<String, OrgIssueTypeRegistry>>,
}

impl InMemoryOrgIssueTypeRepository {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl OrgIssueTypeRepository for InMemoryOrgIssueTypeRepository {
    async fn save(&self, registry: &OrgIssueTypeRegistry) -> PortResult<()> {
        self.by_owner
            .lock()
            .unwrap()
            .insert(registry.owner_login.clone(), registry.clone());
        Ok(())
    }

    async fn get(&self, owner_login: &str) -> PortResult<OrgIssueTypeRegistry> {
        // Absent owner → empty registry (is_available() == false), never
        // NotFound — mirrors the SQLite repo's D8 contract.
        let mut registry = self
            .by_owner
            .lock()
            .unwrap()
            .get(owner_login)
            .cloned()
            .unwrap_or_else(|| OrgIssueTypeRegistry::new(owner_login, Vec::new()));
        // Match SqliteOrgIssueTypeRepository::get's `ORDER BY name` so the two
        // doubles are behaviour-identical on ordering.
        registry.types.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(registry)
    }
}
