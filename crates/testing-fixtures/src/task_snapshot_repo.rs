use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use domain_core::TaskId;
use domain_task::TaskSnapshot;
use ports::{PortError, PortResult, TaskSnapshotRepository};

use crate::task_repo::InMemoryTaskRepository;

// ---------- Task snapshot repository -------------------------------------

pub struct InMemoryTaskSnapshotRepository {
    inner: Arc<Mutex<HashMap<TaskId, Vec<TaskSnapshot>>>>,
}

impl InMemoryTaskSnapshotRepository {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn linked_to(repo: &InMemoryTaskRepository) -> Self {
        Self {
            inner: repo.snapshots_handle(),
        }
    }
}

impl Default for InMemoryTaskSnapshotRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TaskSnapshotRepository for InMemoryTaskSnapshotRepository {
    async fn list(&self, task_id: TaskId) -> PortResult<Vec<TaskSnapshot>> {
        let g = self.inner.lock().unwrap();
        let mut rows = g.get(&task_id).cloned().unwrap_or_default();
        rows.sort_by_key(|s| s.version);
        Ok(rows)
    }

    async fn get(&self, task_id: TaskId, version: u64) -> PortResult<TaskSnapshot> {
        self.inner
            .lock()
            .unwrap()
            .get(&task_id)
            .and_then(|h| h.iter().find(|s| s.version == version).cloned())
            .ok_or_else(|| PortError::NotFound(format!("task {task_id} version {version}")))
    }
}
