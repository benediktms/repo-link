use async_trait::async_trait;
use chrono::{DateTime, Utc};
use domain_core::{ProjectId, Timestamp, WorkspaceId};
use domain_project::{
    FieldOption, PriorityMapping, Project, ProjectField, ProjectFieldKind, StatusMapping,
};
use domain_task::Priority;
use ports::{PortError, PortResult, ProjectRepository};
use sqlx::Row;

use crate::Db;
use crate::mapping::{enum_from_str, enum_to_str, map_sqlx_err};

pub(crate) const PROJECT_COLS: &str =
    "id, provider, owner_login, number, title, archived, created_at, updated_at";

// Same column set as `PROJECT_COLS`, qualified to the `projects` table for use
// in joins where bare names like `id` / `created_at` / `updated_at` collide
// with the joined table (e.g. `workspaces`). Pinning the projection (rather
// than `SELECT projects.*`) keeps `column_count()` constant across a
// cross-process `ALTER TABLE projects ADD COLUMN`, which is the #110 fix.
pub(crate) const PROJECT_COLS_QUALIFIED: &str = "projects.id, projects.provider, projects.owner_login, projects.number, projects.title, projects.archived, projects.created_at, projects.updated_at";

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
        // DELETE-then-INSERT for the field/option child rows can't race with
        // a concurrent reader claiming a stale set. Same trick as
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
                (id, provider, owner_login, number, title, archived, created_at, updated_at)
            VALUES (?, 'github', ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                owner_login = excluded.owner_login,
                number = excluded.number,
                title = excluded.title,
                archived = excluded.archived,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(project.id.as_str())
        .bind(&project.owner_login)
        .bind(
            i64::try_from(project.number)
                .map_err(|e| PortError::Backend(format!("project.number overflow: {e}")))?,
        )
        .bind(&project.title)
        .bind(if project.archived { 1_i64 } else { 0 })
        .bind(project.created_at.into_inner())
        .bind(project.updated_at.into_inner())
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_err)?;

        // Replace the field/option/mapping child rows wholesale. The retained
        // fields are a 100% mirror of the remote definition — diffing locally
        // adds no value and would mishandle renames (same id, different name).
        // Delete in FK-safe child-first order (mappings → options → fields) so
        // the ordering is explicit and doesn't lean on cascade timing, then
        // re-insert parent-first below. Both mapping tables (status + priority)
        // are leaves off `project_field_options`.
        sqlx::query("DELETE FROM project_status_mappings WHERE project_id = ?")
            .bind(project.id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;
        sqlx::query("DELETE FROM project_priority_mappings WHERE project_id = ?")
            .bind(project.id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;
        sqlx::query("DELETE FROM project_field_options WHERE project_id = ?")
            .bind(project.id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;
        sqlx::query("DELETE FROM project_fields WHERE project_id = ?")
            .bind(project.id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;

        // Every retained single-select field, and its option catalog. Fields
        // are inserted before their options to satisfy the composite FK
        // `project_field_options(project_id, field_id) → project_fields`.
        for field in &project.fields {
            sqlx::query(
                r#"
                INSERT INTO project_fields
                    (project_id, field_id, name, kind)
                VALUES (?, ?, ?, ?)
                "#,
            )
            .bind(project.id.as_str())
            .bind(&field.field_id)
            .bind(&field.name)
            .bind(field.kind.as_db_str())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;

            for opt in &field.options {
                sqlx::query(
                    r#"
                    INSERT INTO project_field_options
                        (project_id, field_id, option_id, name, ordinal)
                    VALUES (?, ?, ?, ?, ?)
                    "#,
                )
                .bind(project.id.as_str())
                .bind(&field.field_id)
                .bind(&opt.option_id)
                .bind(&opt.name)
                .bind(i64::from(opt.ordinal))
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx_err)?;
            }
        }

        // Write mappings from `status_mappings` — the domain source of truth.
        // Each row carries the Status field's id so the composite FK
        // `(project_id, field_id, option_id) → project_field_options` holds.
        // The `(project_id, is_open)` PK rejects a duplicate bucket at the DB,
        // matching the `Project` invariant. Mappings only exist when the
        // project has a Status field, so the `status_field_id()` guard never
        // skips a row that should have been written.
        if let Some(status_field_id) = project.status_field_id() {
            for m in &project.status_mappings {
                sqlx::query(
                    r#"
                    INSERT INTO project_status_mappings
                        (project_id, field_id, is_open, option_id)
                    VALUES (?, ?, ?, ?)
                    "#,
                )
                .bind(project.id.as_str())
                .bind(status_field_id)
                .bind(i64::from(m.is_open))
                .bind(&m.option_id)
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx_err)?;
            }
        }

        // Priority mappings (RFC 0006 D3) mirror the status-mapping shape: each
        // row carries the Priority field's id so the composite FK
        // `(project_id, field_id, option_id) → project_field_options` holds, and
        // the `(project_id, priority)` PK rejects a duplicate priority bucket.
        // Mappings only exist when the project has a Priority field, so the
        // guard never skips a row that should have been written.
        if let Some(priority_field_id) = project.priority_field().map(|f| f.field_id.as_str()) {
            for m in &project.priority_mappings {
                sqlx::query(
                    r#"
                    INSERT INTO project_priority_mappings
                        (project_id, field_id, priority, option_id)
                    VALUES (?, ?, ?, ?)
                    "#,
                )
                .bind(project.id.as_str())
                .bind(priority_field_id)
                .bind(enum_to_str(&m.priority)?)
                .bind(&m.option_id)
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx_err)?;
            }
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
        let row = sqlx::query(&format!("SELECT {PROJECT_COLS} FROM projects WHERE id = ?"))
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
        let rows = sqlx::query(&format!(
            r#"
            SELECT {PROJECT_COLS_QUALIFIED}
              FROM projects
              JOIN workspaces ON workspaces.project_id = projects.id
             WHERE workspaces.id = ?
            "#
        ))
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
        let rows = sqlx::query(&format!(
            "SELECT {PROJECT_COLS} FROM projects ORDER BY owner_login, number"
        ))
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
        // `project_fields.project_id` is ON DELETE CASCADE; the option and
        // mapping rows chain off it (`project_field_options` → `project_fields`
        // → and `project_status_mappings` → `project_field_options`, all ON
        // DELETE CASCADE), so deleting the project clears the whole subtree.
        // Workspaces with a `project_id` pointing here are ON DELETE SET NULL —
        // they become projectless.
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
    let archived: i64 = row.try_get("archived").map_err(map_sqlx_err)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(map_sqlx_err)?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at").map_err(map_sqlx_err)?;

    let number_u64 = u64::try_from(number)
        .map_err(|e| PortError::Backend(format!("project.number overflow on load: {e}")))?;

    // Load the retained fields (kind persisted, not re-derived) and each
    // field's option catalog. Field order doesn't affect classification — the
    // Status field is found by `kind` — so order by field_id for a stable read.
    let field_rows = sqlx::query(
        r#"
        SELECT field_id, name, kind
          FROM project_fields
         WHERE project_id = ?
         ORDER BY field_id ASC
        "#,
    )
    .bind(id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx_err)?;

    let mut fields = Vec::with_capacity(field_rows.len());
    for f in field_rows.iter() {
        let field_id: String = f.try_get("field_id").map_err(map_sqlx_err)?;
        let name: String = f.try_get("name").map_err(map_sqlx_err)?;
        let kind_str: String = f.try_get("kind").map_err(map_sqlx_err)?;
        let kind = ProjectFieldKind::from_db_str(&kind_str)
            .map_err(|e| PortError::Backend(format!("decode project field kind: {e}")))?;

        let option_rows = sqlx::query(
            r#"
            SELECT option_id, name, ordinal
              FROM project_field_options
             WHERE project_id = ? AND field_id = ?
             ORDER BY ordinal ASC
            "#,
        )
        .bind(id.as_str())
        .bind(&field_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(map_sqlx_err)?;

        let mut options = Vec::with_capacity(option_rows.len());
        for opt in option_rows.iter() {
            let option_id: String = opt.try_get("option_id").map_err(map_sqlx_err)?;
            let opt_name: String = opt.try_get("name").map_err(map_sqlx_err)?;
            let ordinal_raw: i64 = opt.try_get("ordinal").map_err(map_sqlx_err)?;
            let ordinal = u32::try_from(ordinal_raw)
                .map_err(|e| PortError::Backend(format!("ordinal overflow: {e}")))?;
            options.push(FieldOption {
                option_id,
                name: opt_name,
                ordinal,
            });
        }

        fields.push(ProjectField {
            field_id,
            name,
            kind,
            options,
        });
    }

    // Mappings live in their own table — one row per `(project, is_open)`. Read
    // is_open + option_id; the redundant `field_id` column is ignored on load
    // (it is re-derived from the Status field on save). The `Project`
    // re-validation below re-checks each references an owned option.
    // Order open (is_open=1) before closed (0) for a stable read order.
    let mapping_rows = sqlx::query(
        r#"
        SELECT is_open, option_id
          FROM project_status_mappings
         WHERE project_id = ?
         ORDER BY is_open DESC
        "#,
    )
    .bind(id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx_err)?;

    let mut status_mappings = Vec::with_capacity(mapping_rows.len());
    for m in mapping_rows.iter() {
        let is_open: i64 = m.try_get("is_open").map_err(map_sqlx_err)?;
        let option_id: String = m.try_get("option_id").map_err(map_sqlx_err)?;
        status_mappings.push(StatusMapping {
            is_open: is_open != 0,
            option_id,
        });
    }

    // Priority mappings (RFC 0006 D3) — one row per `(project, priority)`. Like
    // the status mappings, the redundant `field_id` column is ignored on load
    // (re-derived from the Priority field on save); `Project::from_fields`
    // re-validates each references an owned Priority option. Ordered by
    // `priority` (p0 < p1 < p2 < p3) for a stable read.
    let priority_rows = sqlx::query(
        r#"
        SELECT priority, option_id
          FROM project_priority_mappings
         WHERE project_id = ?
         ORDER BY priority ASC
        "#,
    )
    .bind(id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx_err)?;

    let mut priority_mappings = Vec::with_capacity(priority_rows.len());
    for m in priority_rows.iter() {
        let priority_str: String = m.try_get("priority").map_err(map_sqlx_err)?;
        let option_id: String = m.try_get("option_id").map_err(map_sqlx_err)?;
        priority_mappings.push(PriorityMapping {
            priority: enum_from_str::<Priority>("priority", &priority_str)?,
            option_id,
        });
    }

    // Round-trip through `Project::from_fields` so the domain invariants
    // (≤1 Status field, mapping references an owned option, no duplicate
    // bucket) re-validate every load. A corrupted row surfaces as a typed
    // error instead of a silently-skewed `option_id_for` result.
    Project::from_fields(
        id,
        owner_login,
        number_u64,
        title,
        fields,
        status_mappings,
        priority_mappings,
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
