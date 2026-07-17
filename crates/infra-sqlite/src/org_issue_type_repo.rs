//! SQLite-backed [`OrgIssueTypeRepository`] — the org-level native issue-type
//! registry cache (RFC 0006 D5/D8).

use async_trait::async_trait;
use domain_project::{OrgIssueType, OrgIssueTypeRegistry};
use ports::{OrgIssueTypeRepository, PortResult};
use sqlx::Row;

use crate::Db;
use crate::mapping::map_sqlx_err;

/// Explicit projection for `org_issue_types` reads — byte-equal to the table's
/// live column set (drift-checked by `schema_const_consistency`). See the #110
/// rationale on `PROJECT_COLS`.
pub(crate) const ORG_ISSUE_TYPE_COLS: &str = "owner_login, issue_type_id, name";

pub struct SqliteOrgIssueTypeRepository {
    db: Db,
}

impl SqliteOrgIssueTypeRepository {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

#[async_trait]
impl OrgIssueTypeRepository for SqliteOrgIssueTypeRepository {
    async fn save(&self, registry: &OrgIssueTypeRegistry) -> PortResult<()> {
        // BEGIN IMMEDIATE grabs the writer lock up front so the
        // DELETE-then-INSERT can't race a concurrent reader onto a stale set —
        // same replace-wholesale trick as `SqliteProjectRepository::save`.
        let mut tx = self
            .db
            .writes
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_sqlx_err)?;

        sqlx::query("DELETE FROM org_issue_types WHERE owner_login = ?")
            .bind(&registry.owner_login)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;

        for ty in &registry.types {
            sqlx::query(
                r#"
                INSERT INTO org_issue_types (owner_login, issue_type_id, name)
                VALUES (?, ?, ?)
                "#,
            )
            .bind(&registry.owner_login)
            .bind(&ty.issue_type_id)
            .bind(&ty.name)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;
        }

        tx.commit().await.map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn get(&self, owner_login: &str) -> PortResult<OrgIssueTypeRegistry> {
        let rows = sqlx::query(&format!(
            "SELECT {ORG_ISSUE_TYPE_COLS} FROM org_issue_types WHERE owner_login = ? ORDER BY name"
        ))
        .bind(owner_login)
        .fetch_all(&self.db.reads)
        .await
        .map_err(map_sqlx_err)?;

        let mut types = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            let issue_type_id: String = row.try_get("issue_type_id").map_err(map_sqlx_err)?;
            let name: String = row.try_get("name").map_err(map_sqlx_err)?;
            types.push(OrgIssueType {
                issue_type_id,
                name,
            });
        }
        // Zero rows → an empty registry (is_available() == false), never
        // NotFound: the D8 availability signal is an empty set, not an error.
        Ok(OrgIssueTypeRegistry::new(owner_login, types))
    }
}
