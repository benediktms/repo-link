use async_trait::async_trait;
use chrono::{DateTime, Utc};
use domain_core::{ProjectId, RepoId, Timestamp, WorkspaceId};
use domain_workspace::{Workspace, WorkspaceName, WorkspaceStatus};
use ports::{PortError, PortResult, WorkspaceRepository};
use sqlx::Row;

use crate::Db;
use crate::mapping::{enum_from_str, enum_to_str, map_sqlx_err, parse_uuid};

pub struct SqliteWorkspaceRepository {
    db: Db,
}

impl SqliteWorkspaceRepository {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

// Must name every live column (schema-consistency contract, see #110).
// `filing_repo_id` is the RFC 0002 workspace default filing repo (#116).
pub(crate) const WORKSPACE_COLS: &str =
    "id, name, description, status, local_only, created_at, updated_at, project_id, filing_repo_id";

#[async_trait]
impl WorkspaceRepository for SqliteWorkspaceRepository {
    async fn save(&self, w: &Workspace) -> PortResult<()> {
        sqlx::query(
            r#"
            INSERT INTO workspaces (id, name, description, status, local_only, project_id, filing_repo_id, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                description = excluded.description,
                status = excluded.status,
                local_only = excluded.local_only,
                project_id = excluded.project_id,
                filing_repo_id = excluded.filing_repo_id,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(w.id.to_string())
        .bind(w.name.as_str())
        .bind(w.description.as_deref())
        .bind(enum_to_str(&w.status)?)
        .bind(w.local_only as i64)
        .bind(w.project_id.as_ref().map(|p| p.as_str()))
        .bind(w.filing_repo_id.map(|r| r.to_string()))
        .bind(w.created_at.into_inner())
        .bind(w.updated_at.into_inner())
        .execute(&self.db.writes)
        .await
        .map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn get(&self, id: WorkspaceId) -> PortResult<Workspace> {
        let row = sqlx::query(&format!(
            "SELECT {WORKSPACE_COLS} FROM workspaces WHERE id = ?"
        ))
        .bind(id.to_string())
        .fetch_optional(&self.db.reads)
        .await
        .map_err(map_sqlx_err)?
        .ok_or_else(|| PortError::NotFound(format!("workspace {id}")))?;
        row_to_workspace(&row)
    }

    async fn find_by_name(&self, name: &str) -> PortResult<Option<Workspace>> {
        let row = sqlx::query(&format!(
            "SELECT {WORKSPACE_COLS} FROM workspaces WHERE name = ?"
        ))
        .bind(name)
        .fetch_optional(&self.db.reads)
        .await
        .map_err(map_sqlx_err)?;
        row.as_ref().map(row_to_workspace).transpose()
    }

    async fn list(&self, include_archived: bool) -> PortResult<Vec<Workspace>> {
        let sql = if include_archived {
            format!("SELECT {WORKSPACE_COLS} FROM workspaces ORDER BY created_at")
        } else {
            format!(
                "SELECT {WORKSPACE_COLS} FROM workspaces WHERE status NOT IN ('archived','deleted') ORDER BY created_at"
            )
        };
        let rows = sqlx::query(&sql)
            .fetch_all(&self.db.reads)
            .await
            .map_err(map_sqlx_err)?;
        rows.iter().map(row_to_workspace).collect()
    }

    async fn delete(&self, id: WorkspaceId) -> PortResult<()> {
        sqlx::query("DELETE FROM workspaces WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.db.writes)
            .await
            .map_err(map_sqlx_err)?;
        Ok(())
    }
}

fn row_to_workspace(row: &sqlx::sqlite::SqliteRow) -> PortResult<Workspace> {
    let id_str: String = row.try_get("id").map_err(map_sqlx_err)?;
    let name_str: String = row.try_get("name").map_err(map_sqlx_err)?;
    let description: Option<String> = row.try_get("description").map_err(map_sqlx_err)?;
    let status_str: String = row.try_get("status").map_err(map_sqlx_err)?;
    let local_only: i64 = row.try_get("local_only").map_err(map_sqlx_err)?;
    let project_id_raw: Option<String> = row.try_get("project_id").map_err(map_sqlx_err)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(map_sqlx_err)?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at").map_err(map_sqlx_err)?;

    let project_id = project_id_raw
        .map(ProjectId::parse)
        .transpose()
        .map_err(|e| PortError::Backend(format!("decode workspace.project_id: {e}")))?;

    // RFC 0002 default filing repo (internal, #116). NULL = no default.
    let filing_repo_id = row
        .try_get::<Option<String>, _>("filing_repo_id")
        .map_err(map_sqlx_err)?
        .as_deref()
        .map(|s| parse_uuid::<RepoId>("filing_repo_id", s))
        .transpose()?;

    Ok(Workspace {
        id: parse_uuid::<WorkspaceId>("workspace_id", &id_str)?,
        name: WorkspaceName::new(&name_str)
            .map_err(|e| PortError::Backend(format!("invalid stored workspace name: {e}")))?,
        description,
        status: enum_from_str::<WorkspaceStatus>("workspace status", &status_str)?,
        local_only: local_only != 0,
        project_id,
        filing_repo_id,
        created_at: Timestamp::from_utc(created_at),
        updated_at: Timestamp::from_utc(updated_at),
    })
}
