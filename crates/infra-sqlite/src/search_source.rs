//! infra-sqlite — RFC 0007 authoritative task/comment search source adapter.
//!
//! `SqliteTaskSearchSourceRepository` reads current task/comment raw text from
//! `repo-link.db` on the existing read pool, and produces a coherent in-memory
//! result snapshot for the literal lane + response assembly (RFC 0007 D4/D6).

use async_trait::async_trait;
use domain_core::{RepoId, TaskId, WorkspaceId};
use ports::{
    CommentTextRow, PortError, SearchScope, TaskIdentity, TaskSearchResultSnapshot,
    TaskSearchSourceRepository, TaskTextRow,
};
use sqlx::Row;
use std::str::FromStr;

use crate::pool::Db;

/// Reads current task/comment search content from the authoritative database.
#[derive(Clone)]
pub struct SqliteTaskSearchSourceRepository {
    db: Db,
}

impl SqliteTaskSearchSourceRepository {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

#[async_trait]
impl TaskSearchSourceRepository for SqliteTaskSearchSourceRepository {
    async fn load_reconcile_snapshot(&self) -> Result<Vec<TaskTextRow>, PortError> {
        let mut conn = self.db.reads.acquire().await.map_err(map_sqlx_err)?;
        load_all_tasks_on(&mut conn).await
    }

    async fn begin_result_snapshot(
        &self,
        scope: &SearchScope,
    ) -> Result<Box<dyn TaskSearchResultSnapshot>, PortError> {
        // One read connection binds the task/comment/identity reads to a
        // single SQLite snapshot (RFC 0007 D4): a concurrent sync cannot mix
        // an old task row with new workspace/display-id metadata.
        let mut conn = self.db.reads.acquire().await.map_err(map_sqlx_err)?;
        let rows = load_all_tasks_on(&mut conn).await?;
        let eligible = rows
            .into_iter()
            .filter(|r| scope_matches(scope, r))
            .collect::<Vec<_>>();
        let snap = build_snapshot_on(&mut conn, eligible).await?;
        Ok(Box::new(snap))
    }
}

fn scope_matches(scope: &SearchScope, r: &TaskTextRow) -> bool {
    if let Some(w) = &scope.workspace_id
        && &r.workspace_id != w
    {
        return false;
    }
    if let Some(rep) = &scope.repo_id
        && r.repo_id.as_ref() != Some(rep)
    {
        return false;
    }
    if let Some(open) = scope.is_open
        && r.is_open != open
    {
        return false;
    }
    true
}

/// Load every current task + comment raw text (RFC 0007 D1 corpus) on the
/// given read connection (a single snapshot).
async fn load_all_tasks_on(
    conn: &mut sqlx::sqlite::SqliteConnection,
) -> Result<Vec<TaskTextRow>, PortError> {
    let trows = sqlx::query(
        "SELECT id, workspace_id, repo_instance_id, lifecycle, title, body \
         FROM tasks ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(map_sqlx_err)?;

    let crows = sqlx::query(
        "SELECT task_id, remote_comment_id, body \
         FROM task_comments ORDER BY created_at",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(map_sqlx_err)?;

    let mut by_task: std::collections::HashMap<TaskId, Vec<CommentTextRow>> =
        std::collections::HashMap::new();
    for r in crows {
        let task_id: TaskId = TaskId::from_str(
            r.try_get::<String, _>("task_id")
                .map_err(map_sqlx_err)?
                .as_str(),
        )
        .map_err(|e| PortError::Backend(e.to_string()))?;
        let remote_comment_id: String = r.try_get("remote_comment_id").map_err(map_sqlx_err)?;
        let body: String = r.try_get("body").map_err(map_sqlx_err)?;
        by_task.entry(task_id).or_default().push(CommentTextRow {
            remote_comment_id: if remote_comment_id.is_empty() {
                None
            } else {
                Some(remote_comment_id)
            },
            body,
        });
    }

    let mut out = Vec::with_capacity(trows.len());
    for r in trows {
        let task_id: TaskId =
            TaskId::from_str(r.try_get::<String, _>("id").map_err(map_sqlx_err)?.as_str())
                .map_err(|e| PortError::Backend(e.to_string()))?;
        let workspace_id = parse_workspace(
            &r.try_get::<String, _>("workspace_id")
                .map_err(map_sqlx_err)?,
        )?;
        let repo_id: Option<RepoId> = match r
            .try_get::<Option<String>, _>("repo_instance_id")
            .map_err(map_sqlx_err)?
        {
            Some(s) => Some(RepoId::from_str(&s).map_err(|e| PortError::Backend(e.to_string()))?),
            None => None,
        };
        let lifecycle: String = r.try_get("lifecycle").map_err(map_sqlx_err)?;
        let title: String = r.try_get("title").map_err(map_sqlx_err)?;
        let body: String = r.try_get("body").map_err(map_sqlx_err)?;
        out.push(TaskTextRow {
            task_id,
            workspace_id,
            repo_id,
            is_open: lifecycle_is_open(&lifecycle),
            title,
            body,
            comments: by_task.remove(&task_id).unwrap_or_default(),
        });
    }
    Ok(out)
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, PortError> {
    WorkspaceId::from_str(s).map_err(|e| PortError::Backend(e.to_string()))
}

fn lifecycle_is_open(lifecycle: &str) -> bool {
    matches!(lifecycle, "open" | "reopened")
}

/// Load identity fields (display id + workspace name) for all tasks in one
/// query on the given read connection (RFC 0007 D4 "one read snapshot").
async fn build_snapshot_on(
    conn: &mut sqlx::sqlite::SqliteConnection,
    tasks: Vec<TaskTextRow>,
) -> Result<InMemorySnapshot, PortError> {
    let ids: Vec<String> = tasks.iter().map(|t| t.task_id.to_string()).collect();
    if ids.is_empty() {
        return Ok(InMemorySnapshot { items: Vec::new() });
    }
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT t.id AS id, w.name AS ws_name, ro.prefix AS repo_prefix, t.hash AS hash \
         FROM tasks t \
         LEFT JOIN workspaces w ON w.id = t.workspace_id \
         LEFT JOIN repo_instances ri ON ri.id = t.repo_instance_id \
         LEFT JOIN repo_origins ro ON ro.id = ri.origin_id \
         WHERE t.id IN ({placeholders})"
    );
    let mut q = sqlx::query(&sql);
    for id in &ids {
        q = q.bind(id);
    }
    let rows = q.fetch_all(&mut *conn).await.map_err(map_sqlx_err)?;
    let mut by_id: std::collections::HashMap<TaskId, (String, String)> = Default::default();
    for r in rows {
        let Ok(tid) =
            TaskId::from_str(r.try_get::<String, _>("id").map_err(map_sqlx_err)?.as_str())
        else {
            continue;
        };
        let ws_name: String = r.try_get("ws_name").unwrap_or_default();
        let prefix: String = r.try_get("repo_prefix").unwrap_or_default();
        let hash: String = r.try_get("hash").unwrap_or_default();
        let display_id = if prefix.is_empty() {
            hash.clone()
        } else {
            format!("{prefix}-{hash}")
        };
        by_id.insert(tid, (display_id, ws_name));
    }
    let items = tasks
        .into_iter()
        .map(|task| {
            let (display_id, workspace_name) = by_id
                .remove(&task.task_id)
                .unwrap_or((task.task_id.to_string(), String::new()));
            SnapshotItem {
                task,
                display_id,
                workspace_name,
            }
        })
        .collect();
    Ok(InMemorySnapshot { items })
}

/// In-memory authoritative result snapshot (RFC 0007 D4 "one read snapshot").
struct InMemorySnapshot {
    items: Vec<SnapshotItem>,
}

struct SnapshotItem {
    task: TaskTextRow,
    display_id: String,
    workspace_name: String,
}

#[async_trait]
impl TaskSearchResultSnapshot for InMemorySnapshot {
    async fn eligible_rows(&self) -> Result<Vec<TaskTextRow>, PortError> {
        Ok(self.items.iter().map(|i| i.task.clone()).collect())
    }

    async fn verify_sources(&self, task_ids: &[TaskId]) -> Result<Vec<TaskId>, PortError> {
        let present: std::collections::HashSet<TaskId> =
            self.items.iter().map(|i| i.task.task_id).collect();
        Ok(task_ids
            .iter()
            .copied()
            .filter(|t| present.contains(t))
            .collect())
    }

    async fn task_identity(&self, task_id: &TaskId) -> Result<TaskIdentity, PortError> {
        let item = self
            .items
            .iter()
            .find(|i| i.task.task_id == *task_id)
            .ok_or_else(|| PortError::NotFound(task_id.to_string()))?;
        Ok(TaskIdentity {
            display_id: item.display_id.clone(),
            workspace_id: item.task.workspace_id,
            workspace_name: item.workspace_name.clone(),
            title: item.task.title.clone(),
        })
    }
}

fn map_sqlx_err(e: sqlx::Error) -> PortError {
    PortError::Backend(e.to_string())
}
