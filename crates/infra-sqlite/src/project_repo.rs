use async_trait::async_trait;
use chrono::{DateTime, Utc};
use domain_core::{ProjectId, Timestamp, WorkspaceId};
use domain_project::{Project, StatusMapping, StatusOption};
use domain_task::TaskStatus;
use ports::{PortError, PortResult, ProjectRepository};
use sqlx::Row;

use crate::Db;
use crate::mapping::{enum_from_str, enum_to_str, map_sqlx_err};

pub struct SqliteProjectRepository {
    db: Db,
}

impl SqliteProjectRepository {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

#[async_trait]
impl ProjectRepository for SqliteProjectRepository {
    async fn save(&self, project: &Project) -> PortResult<()> {
        // BEGIN IMMEDIATE grabs the writer lock up front so the
        // DELETE-then-INSERT for `project_status_options` can't race with
        // a concurrent reader claiming a stale option set. Same trick as
        // `SqliteRepoBindingRepository::save`.
        let mut tx = self
            .db
            .writes
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_sqlx_err)?;

        sqlx::query(
            r#"
            INSERT INTO projects
                (id, provider, owner_login, number, title, status_field_id, archived, created_at, updated_at)
            VALUES (?, 'github', ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                owner_login = excluded.owner_login,
                number = excluded.number,
                title = excluded.title,
                status_field_id = excluded.status_field_id,
                archived = excluded.archived,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(project.id.as_str())
        .bind(&project.owner_login)
        .bind(i64::try_from(project.number).map_err(|e| {
            PortError::Backend(format!("project.number overflow: {e}"))
        })?)
        .bind(&project.title)
        .bind(&project.status_field_id)
        .bind(if project.archived { 1_i64 } else { 0 })
        .bind(project.created_at.into_inner())
        .bind(project.updated_at.into_inner())
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_err)?;

        // Replace the option set wholesale. Options are a 100% mirror of
        // the remote field definition — diffing locally adds no value and
        // would mishandle renames (same option_id, different name).
        sqlx::query("DELETE FROM project_status_options WHERE project_id = ?")
            .bind(project.id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;

        for opt in &project.status_options {
            let default_for = project
                .status_mappings
                .iter()
                .find(|m| m.option_id == opt.option_id)
                .map(|m| enum_to_str(&m.status))
                .transpose()?;
            sqlx::query(
                r#"
                INSERT INTO project_status_options
                    (project_id, option_id, name, default_for, ordinal)
                VALUES (?, ?, ?, ?, ?)
                "#,
            )
            .bind(project.id.as_str())
            .bind(&opt.option_id)
            .bind(&opt.name)
            .bind(default_for)
            .bind(i64::from(opt.ordinal))
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;
        }

        tx.commit().await.map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn get(&self, id: ProjectId) -> PortResult<Project> {
        // Read the project row and its option catalog inside one transaction
        // so a concurrent writer commit between the two queries can't return
        // torn state (project metadata from snapshot A, options from snapshot
        // B). SQLite WAL gives the transaction a single consistent snapshot.
        let mut tx = self.db.reads.begin().await.map_err(map_sqlx_err)?;
        let row = sqlx::query("SELECT * FROM projects WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx_err)?
            .ok_or_else(|| PortError::NotFound(format!("project {id}")))?;
        let project = row_to_project(&row, &mut tx).await?;
        tx.commit().await.map_err(map_sqlx_err)?;
        Ok(project)
    }

    async fn list_by_workspace(&self, ws: WorkspaceId) -> PortResult<Vec<Project>> {
        let mut tx = self.db.reads.begin().await.map_err(map_sqlx_err)?;
        let rows = sqlx::query(
            r#"
            SELECT projects.*
              FROM projects
              JOIN workspaces ON workspaces.project_id = projects.id
             WHERE workspaces.id = ?
            "#,
        )
        .bind(ws.to_string())
        .fetch_all(&mut *tx)
        .await
        .map_err(map_sqlx_err)?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            out.push(row_to_project(row, &mut tx).await?);
        }
        tx.commit().await.map_err(map_sqlx_err)?;
        Ok(out)
    }

    async fn list_all(&self) -> PortResult<Vec<Project>> {
        let mut tx = self.db.reads.begin().await.map_err(map_sqlx_err)?;
        let rows = sqlx::query("SELECT * FROM projects ORDER BY owner_login, number")
            .fetch_all(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            out.push(row_to_project(row, &mut tx).await?);
        }
        tx.commit().await.map_err(map_sqlx_err)?;
        Ok(out)
    }

    async fn delete(&self, id: ProjectId) -> PortResult<()> {
        // `project_status_options.project_id` is ON DELETE CASCADE, so the
        // option rows clear automatically. Workspaces with a `project_id`
        // pointing here are ON DELETE SET NULL — they become projectless.
        sqlx::query("DELETE FROM projects WHERE id = ?")
            .bind(id.as_str())
            .execute(&self.db.writes)
            .await
            .map_err(map_sqlx_err)?;
        Ok(())
    }
}

async fn row_to_project(
    row: &sqlx::sqlite::SqliteRow,
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> PortResult<Project> {
    let id_str: String = row.try_get("id").map_err(map_sqlx_err)?;
    let id = ProjectId::parse(id_str.clone())
        .map_err(|e| PortError::Backend(format!("parse project id {id_str:?}: {e}")))?;
    let owner_login: String = row.try_get("owner_login").map_err(map_sqlx_err)?;
    let number: i64 = row.try_get("number").map_err(map_sqlx_err)?;
    let title: String = row.try_get("title").map_err(map_sqlx_err)?;
    let status_field_id: String = row.try_get("status_field_id").map_err(map_sqlx_err)?;
    let archived: i64 = row.try_get("archived").map_err(map_sqlx_err)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(map_sqlx_err)?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at").map_err(map_sqlx_err)?;

    let number_u64 = u64::try_from(number)
        .map_err(|e| PortError::Backend(format!("project.number overflow on load: {e}")))?;

    let option_rows = sqlx::query(
        r#"
        SELECT option_id, name, default_for, ordinal
          FROM project_status_options
         WHERE project_id = ?
         ORDER BY ordinal ASC
        "#,
    )
    .bind(id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx_err)?;

    let mut status_options = Vec::with_capacity(option_rows.len());
    let mut status_mappings = Vec::new();
    for opt in option_rows.iter() {
        let option_id: String = opt.try_get("option_id").map_err(map_sqlx_err)?;
        let name: String = opt.try_get("name").map_err(map_sqlx_err)?;
        let default_for: Option<String> = opt.try_get("default_for").map_err(map_sqlx_err)?;
        let ordinal_raw: i64 = opt.try_get("ordinal").map_err(map_sqlx_err)?;
        let ordinal = u32::try_from(ordinal_raw)
            .map_err(|e| PortError::Backend(format!("ordinal overflow: {e}")))?;

        if let Some(status_str) = default_for {
            status_mappings.push(StatusMapping {
                status: enum_from_str::<TaskStatus>(
                    "project_status_options.default_for",
                    &status_str,
                )?,
                option_id: option_id.clone(),
            });
        }
        status_options.push(StatusOption {
            option_id,
            name,
            ordinal,
        });
    }

    // Round-trip through `Project::new` so the domain invariants
    // (mapping references owned option, no duplicate `status`) re-validate
    // every load. A corrupted row surfaces as a typed error instead of a
    // silently-skewed `option_id_for` result.
    Project::new(
        id,
        owner_login,
        number_u64,
        title,
        status_field_id,
        status_options,
        status_mappings,
        archived != 0,
        Timestamp::from_utc(created_at),
    )
    .map(|mut p| {
        // `new` sets created_at = updated_at = now; restore the persisted
        // timestamps so callers see the real history.
        p.created_at = Timestamp::from_utc(created_at);
        p.updated_at = Timestamp::from_utc(updated_at);
        p
    })
    .map_err(|e| PortError::Backend(format!("decode project: {e}")))
}
