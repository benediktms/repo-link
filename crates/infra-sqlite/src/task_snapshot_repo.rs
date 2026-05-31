use async_trait::async_trait;
use chrono::{DateTime, Utc};
use domain_core::{RepoId, TaskId, Timestamp};
use domain_task::{Priority, RemoteRef, SnapshotSource, SyncState, TaskSnapshot, TaskStatus};
use ports::{PortError, PortResult, TaskSnapshotRepository};
use sqlx::Row;

use crate::Db;
use crate::mapping::{enum_from_str, json_from_string, map_sqlx_err, parse_uuid};

pub struct SqliteTaskSnapshotRepository {
    db: Db,
}

impl SqliteTaskSnapshotRepository {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

pub(crate) const TASK_SNAPSHOT_COLS: &str = "task_id, version, title, body, status, sync_state, priority, assignees_json, remote_provider, remote_id, repo_id, repo_id_recorded, filing_repo_id, source, captured_at";

#[async_trait]
impl TaskSnapshotRepository for SqliteTaskSnapshotRepository {
    async fn list(&self, task_id: TaskId) -> PortResult<Vec<TaskSnapshot>> {
        let rows = sqlx::query(&format!(
            "SELECT {TASK_SNAPSHOT_COLS} FROM task_snapshots WHERE task_id = ? ORDER BY version ASC"
        ))
        .bind(task_id.to_string())
        .fetch_all(&self.db.reads)
        .await
        .map_err(map_sqlx_err)?;

        rows.iter()
            .map(|row| row_to_snapshot(task_id, row))
            .collect()
    }

    async fn get(&self, task_id: TaskId, version: u64) -> PortResult<TaskSnapshot> {
        let version_i64 = i64::try_from(version)
            .map_err(|e| PortError::Backend(format!("snapshot version overflow: {e}")))?;
        let row = sqlx::query(&format!(
            "SELECT {TASK_SNAPSHOT_COLS} FROM task_snapshots WHERE task_id = ? AND version = ?"
        ))
        .bind(task_id.to_string())
        .bind(version_i64)
        .fetch_optional(&self.db.reads)
        .await
        .map_err(map_sqlx_err)?
        .ok_or_else(|| PortError::NotFound(format!("task {task_id} version {version}")))?;

        row_to_snapshot(task_id, &row)
    }
}

fn row_to_snapshot(task_id: TaskId, row: &sqlx::sqlite::SqliteRow) -> PortResult<TaskSnapshot> {
    let id_str: String = row.try_get("task_id").map_err(map_sqlx_err)?;
    let version: i64 = row.try_get("version").map_err(map_sqlx_err)?;
    let title: String = row.try_get("title").map_err(map_sqlx_err)?;
    let body: String = row.try_get("body").map_err(map_sqlx_err)?;
    let status: String = row.try_get("status").map_err(map_sqlx_err)?;
    let sync_state: String = row.try_get("sync_state").map_err(map_sqlx_err)?;
    let priority: String = row.try_get("priority").map_err(map_sqlx_err)?;
    let assignees_json: String = row.try_get("assignees_json").map_err(map_sqlx_err)?;
    let remote_provider: Option<String> = row.try_get("remote_provider").map_err(map_sqlx_err)?;
    let remote_id: Option<String> = row.try_get("remote_id").map_err(map_sqlx_err)?;
    let repo_id_raw: Option<String> = row.try_get("repo_id").map_err(map_sqlx_err)?;
    let repo_id_recorded_raw: i64 = row.try_get("repo_id_recorded").map_err(map_sqlx_err)?;
    let filing_repo_id_raw: Option<String> = row.try_get("filing_repo_id").map_err(map_sqlx_err)?;
    let source: String = row.try_get("source").map_err(map_sqlx_err)?;
    let captured_at: DateTime<Utc> = row.try_get("captured_at").map_err(map_sqlx_err)?;

    let _ = parse_uuid::<TaskId>("task_id", &id_str)?;

    let remote = match (remote_provider, remote_id) {
        (Some(provider), Some(remote_id)) => Some(RemoteRef::new(provider, remote_id)),
        _ => None,
    };
    let repo_id = repo_id_raw
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<RepoId>())
        .transpose()
        .map_err(|e: domain_core::IdParseError| PortError::Backend(e.to_string()))?;
    // RFC 0002 #118: history/audit only, NOT restored on rollback. Pre-column
    // rows (added by 20260531000001) read back NULL/empty → None, tolerated via
    // the same non-empty/parse path as `repo_id`.
    let filing_repo_id = filing_repo_id_raw
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<RepoId>())
        .transpose()
        .map_err(|e: domain_core::IdParseError| PortError::Backend(e.to_string()))?;

    let version_u64 = u64::try_from(version)
        .map_err(|e| PortError::Backend(format!("snapshot version overflow: {e}")))?;

    Ok(TaskSnapshot {
        task_id,
        version: version_u64,
        title,
        body,
        status: enum_from_str::<TaskStatus>("task status", &status)?,
        sync_state: enum_from_str::<SyncState>("task sync_state", &sync_state)?,
        priority: enum_from_str::<Priority>("priority", &priority)?,
        assignees: json_from_string::<Vec<String>>("assignees", &assignees_json)?,
        remote,
        repo_id,
        repo_id_recorded: repo_id_recorded_raw != 0,
        filing_repo_id,
        source: enum_from_str::<SnapshotSource>("snapshot source", &source)?,
        captured_at: Timestamp::from_utc(captured_at),
    })
}
