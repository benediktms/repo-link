use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use domain_core::{RepoId, TaskId, Timestamp, WorkspaceId};
use domain_task::{SnapshotSource, Task, TaskComment, TaskSnapshot};
use ports::{PortError, PortResult, RemoteComment, TaskFilter, TaskRepository};

// ---------- Task repository -----------------------------------------------

pub struct InMemoryTaskRepository {
    inner: Mutex<HashMap<TaskId, Task>>,
    snapshots: Arc<Mutex<HashMap<TaskId, Vec<TaskSnapshot>>>>,
    // Comments live in their own store (like the `task_comments` table), so
    // `save` never clobbers them and reads overlay the current set.
    comments: Mutex<HashMap<TaskId, Vec<TaskComment>>>,
}

impl InMemoryTaskRepository {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            snapshots: Arc::new(Mutex::new(HashMap::new())),
            comments: Mutex::new(HashMap::new()),
        }
    }

    pub fn snapshots_handle(&self) -> Arc<Mutex<HashMap<TaskId, Vec<TaskSnapshot>>>> {
        Arc::clone(&self.snapshots)
    }
}

impl Default for InMemoryTaskRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TaskRepository for InMemoryTaskRepository {
    async fn save(&self, task: &Task, source: SnapshotSource) -> PortResult<()> {
        let mut snaps = self.snapshots.lock().unwrap();
        let history = snaps.entry(task.id).or_default();
        let next_version = history.iter().map(|s| s.version).max().unwrap_or(0) + 1;
        history.push(TaskSnapshot {
            task_id: task.id,
            version: next_version,
            title: task.title.clone(),
            body: task.body.clone(),
            status: task.status,
            sync_state: task.sync,
            priority: task.priority,
            assignees: task.assignees.clone(),
            remote: task.remote.clone(),
            repo_id: task.repo_id,
            repo_id_recorded: true,
            source,
            captured_at: Timestamp::now(),
        });
        drop(snaps);
        self.inner.lock().unwrap().insert(task.id, task.clone());
        Ok(())
    }

    async fn get(&self, id: TaskId) -> PortResult<Task> {
        let mut task = self
            .inner
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| PortError::NotFound(format!("task {id}")))?;
        let snaps = self.snapshots.lock().unwrap();
        task.synced_baseline = snaps
            .get(&id)
            .and_then(|h| h.iter().rfind(|s| s.is_baseline()).cloned());
        task.comments = self
            .comments
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .unwrap_or_default();
        Ok(task)
    }

    async fn list(&self, filter: TaskFilter) -> PortResult<Vec<Task>> {
        let g = self.inner.lock().unwrap();
        let snaps = self.snapshots.lock().unwrap();
        let mut rows: Vec<_> = g
            .values()
            .filter(|t| match filter.workspace_id {
                Some(w) => t.workspace_id == w,
                None => true,
            })
            .filter(|t| match filter.repo_id {
                Some(r) => t.repo_id == Some(r),
                None => true,
            })
            .filter(|t| match filter.status {
                Some(s) => t.status == s,
                None => true,
            })
            .filter(|t| match filter.sync_state {
                Some(s) => t.sync == s,
                None => true,
            })
            .filter(|t| match (filter.status, filter.include_archived) {
                // Explicit status filter is authoritative.
                (Some(_), _) => true,
                // No status filter + include_archived=false → exclude Archived.
                (None, false) => t.status != domain_task::TaskStatus::Archived,
                (None, true) => true,
            })
            .map(|t| {
                let mut task = t.clone();
                task.synced_baseline = snaps
                    .get(&t.id)
                    .and_then(|h| h.iter().rfind(|s| s.is_baseline()).cloned());
                task
            })
            .collect();
        rows.sort_by_key(|t| t.created_at);
        Ok(rows)
    }

    async fn find_by_hash(&self, hash: &str) -> PortResult<Option<Task>> {
        if hash.is_empty() {
            return Ok(None);
        }
        let g = self.inner.lock().unwrap();
        let Some(task) = g.values().find(|t| t.hash == hash).cloned() else {
            return Ok(None);
        };
        // Restore the synced_baseline projection the same way `get` does.
        let snaps = self.snapshots.lock().unwrap();
        let mut task = task;
        task.synced_baseline = snaps
            .get(&task.id)
            .and_then(|h| h.iter().rfind(|s| s.is_baseline()).cloned());
        task.comments = self
            .comments
            .lock()
            .unwrap()
            .get(&task.id)
            .cloned()
            .unwrap_or_default();
        Ok(Some(task))
    }

    async fn find_by_remote(
        &self,
        repo_id: RepoId,
        provider: &str,
        remote_id: &str,
    ) -> PortResult<Option<Task>> {
        let g = self.inner.lock().unwrap();
        let Some(task) = g
            .values()
            .find(|t| {
                t.repo_id == Some(repo_id)
                    && t.remote
                        .as_ref()
                        .is_some_and(|r| r.provider == provider && r.remote_id == remote_id)
            })
            .cloned()
        else {
            return Ok(None);
        };
        let snaps = self.snapshots.lock().unwrap();
        let mut task = task;
        task.synced_baseline = snaps
            .get(&task.id)
            .and_then(|h| h.iter().rfind(|s| s.is_baseline()).cloned());
        task.comments = self
            .comments
            .lock()
            .unwrap()
            .get(&task.id)
            .cloned()
            .unwrap_or_default();
        Ok(Some(task))
    }

    async fn replace_comments(
        &self,
        task_id: TaskId,
        comments: &[RemoteComment],
    ) -> PortResult<()> {
        let mut store = self.comments.lock().unwrap();
        let entry = store.entry(task_id).or_default();
        // Keep pending (local-only) comments; replace the synced set.
        let mut next: Vec<TaskComment> = entry
            .iter()
            .filter(|c| c.remote_id.is_none())
            .cloned()
            .collect();
        next.extend(comments.iter().map(|c| TaskComment {
            local_id: Some(uuid::Uuid::new_v4().to_string()),
            remote_id: Some(c.remote_id.clone()),
            author: c.author.clone(),
            body: c.body.clone(),
            created_at: c.created_at,
        }));
        *entry = next;
        Ok(())
    }

    async fn add_pending_comment(
        &self,
        task_id: TaskId,
        author: &str,
        body: &str,
        created_at: Timestamp,
    ) -> PortResult<()> {
        let mut store = self.comments.lock().unwrap();
        store.entry(task_id).or_default().push(TaskComment {
            local_id: Some(uuid::Uuid::new_v4().to_string()),
            remote_id: None,
            author: author.to_string(),
            body: body.to_string(),
            created_at,
        });
        Ok(())
    }

    async fn mark_comments_pushed(
        &self,
        task_id: TaskId,
        drained_local_ids: &[String],
        pushed: &[RemoteComment],
    ) -> PortResult<()> {
        let mut store = self.comments.lock().unwrap();
        let entry = store.entry(task_id).or_default();
        // Identity-aware drain: drop only the rows whose local_id was actually
        // pushed; newly-added pending comments are preserved. Append the
        // freshly-pushed comments as synced.
        let drained: std::collections::HashSet<&str> =
            drained_local_ids.iter().map(String::as_str).collect();
        let mut next: Vec<TaskComment> = entry
            .iter()
            .filter(|c| !c.local_id.as_deref().is_some_and(|id| drained.contains(id)))
            .cloned()
            .collect();
        next.extend(pushed.iter().map(|c| TaskComment {
            local_id: Some(uuid::Uuid::new_v4().to_string()),
            remote_id: Some(c.remote_id.clone()),
            author: c.author.clone(),
            body: c.body.clone(),
            created_at: c.created_at,
        }));
        *entry = next;
        Ok(())
    }

    async fn pending_comment_counts(
        &self,
        workspace_id: WorkspaceId,
    ) -> PortResult<std::collections::HashMap<TaskId, usize>> {
        let tasks = self.inner.lock().unwrap();
        let comments = self.comments.lock().unwrap();
        let mut out = std::collections::HashMap::new();
        for (task_id, cs) in comments.iter() {
            let in_ws = tasks
                .get(task_id)
                .is_some_and(|t| t.workspace_id == workspace_id);
            if !in_ws {
                continue;
            }
            let n = cs.iter().filter(|c| c.remote_id.is_none()).count();
            if n > 0 {
                out.insert(*task_id, n);
            }
        }
        Ok(out)
    }

    async fn delete(&self, id: TaskId) -> PortResult<()> {
        self.inner.lock().unwrap().remove(&id);
        Ok(())
    }
}
