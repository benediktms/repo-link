use async_trait::async_trait;
use chrono::{DateTime, Utc};
use domain_core::{ProjectId, Timestamp, WorkspaceId};
use domain_project::{Project, StatusMapping, StatusOption};
use domain_task::TaskStatus;
use ports::{PortError, PortResult, ProjectRepository};
use sqlx::Row;

use crate::Db;
use crate::mapping::{enum_from_str, enum_to_str, map_sqlx_err};

pub(crate) const PROJECT_COLS: &str =
    "id, provider, owner_login, number, title, status_field_id, archived, created_at, updated_at";

// Same column set as `PROJECT_COLS`, qualified to the `projects` table for use
// in joins where bare names like `id` / `created_at` / `updated_at` collide
// with the joined table (e.g. `workspaces`). Pinning the projection (rather
// than `SELECT projects.*`) keeps `column_count()` constant across a
// cross-process `ALTER TABLE projects ADD COLUMN`, which is the #110 fix.
pub(crate) const PROJECT_COLS_QUALIFIED: &str = "projects.id, projects.provider, projects.owner_login, projects.number, projects.title, projects.status_field_id, projects.archived, projects.created_at, projects.updated_at";

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
        // would mishandle renames (same option_id, different name). The
        // mappings rows FK onto options with ON DELETE CASCADE, so clearing
        // options also clears the project's mappings; we re-insert both from
        // the domain object below. Delete mappings first anyway so the
        // ordering is explicit and doesn't lean on cascade timing.
        sqlx::query("DELETE FROM project_status_mappings WHERE project_id = ?")
            .bind(project.id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;
        sqlx::query("DELETE FROM project_status_options WHERE project_id = ?")
            .bind(project.id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;

        for opt in &project.status_options {
            sqlx::query(
                r#"
                INSERT INTO project_status_options
                    (project_id, option_id, name, ordinal)
                VALUES (?, ?, ?, ?)
                "#,
            )
            .bind(project.id.as_str())
            .bind(&opt.option_id)
            .bind(&opt.name)
            .bind(i64::from(opt.ordinal))
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;
        }

        // Write mappings from `status_mappings` — the domain source of truth
        // — rather than reverse-deriving a single `default_for` per option.
        // That's the whole point of the dedicated table: a `(status,
        // option_id)` pair per mapping means many statuses can share one
        // option (Open + Blocked → "Backlog") without loss. The
        // `(project_id, status)` PK rejects a duplicate status at the DB,
        // matching the `Project::new` invariant. Options are all inserted
        // above, so the composite FK is satisfied.
        for m in &project.status_mappings {
            sqlx::query(
                r#"
                INSERT INTO project_status_mappings
                    (project_id, status, option_id)
                VALUES (?, ?, ?)
                "#,
            )
            .bind(project.id.as_str())
            .bind(enum_to_str(&m.status)?)
            .bind(&m.option_id)
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
        // `project_status_options.project_id` and
        // `project_status_mappings.project_id` are both ON DELETE CASCADE, so
        // the option and mapping rows clear automatically. Workspaces with a
        // `project_id` pointing here are ON DELETE SET NULL — they become
        // projectless.
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
        SELECT option_id, name, ordinal
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
    for opt in option_rows.iter() {
        let option_id: String = opt.try_get("option_id").map_err(map_sqlx_err)?;
        let name: String = opt.try_get("name").map_err(map_sqlx_err)?;
        let ordinal_raw: i64 = opt.try_get("ordinal").map_err(map_sqlx_err)?;
        let ordinal = u32::try_from(ordinal_raw)
            .map_err(|e| PortError::Backend(format!("ordinal overflow: {e}")))?;
        status_options.push(StatusOption {
            option_id,
            name,
            ordinal,
        });
    }

    // Mappings live in their own table now — one row per `(project, status)`,
    // many of which may share one `option_id`. Read them all; the `Project`
    // re-validation below re-checks they reference an owned option.
    //
    // Order by workflow position (Open → InProgress → Blocked → Done) so the
    // load is deterministic. It matters downstream: `project_to_dto` picks
    // the *first* mapping per option for the inline `default_for` field, so a
    // many-to-one option (e.g. Open + Blocked → "Backlog") would otherwise
    // surface an unstable status across reads. Lowest workflow status wins,
    // which reads as the option's primary status.
    let mapping_rows = sqlx::query(
        r#"
        SELECT status, option_id
          FROM project_status_mappings
         WHERE project_id = ?
         ORDER BY CASE status
             WHEN 'open'        THEN 0
             WHEN 'in_progress' THEN 1
             WHEN 'blocked'     THEN 2
             WHEN 'done'        THEN 3
         END
        "#,
    )
    .bind(id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx_err)?;

    let mut status_mappings = Vec::with_capacity(mapping_rows.len());
    for m in mapping_rows.iter() {
        let status_str: String = m.try_get("status").map_err(map_sqlx_err)?;
        let option_id: String = m.try_get("option_id").map_err(map_sqlx_err)?;
        status_mappings.push(StatusMapping {
            status: enum_from_str::<TaskStatus>("project_status_mappings.status", &status_str)?,
            option_id,
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
