use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use domain_core::{RepoId, Timestamp, WorkspaceId};
use domain_repo::{LinkStatus, RepoBinding, WorktreeLink};
use ports::{PortError, PortResult, RepoBindingRepository};
use sqlx::{Row, SqlitePool};

use crate::Db;
use crate::mapping::{enum_from_str, enum_to_str, map_sqlx_err, parse_uuid};

pub struct SqliteRepoBindingRepository {
    db: Db,
}

impl SqliteRepoBindingRepository {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

#[async_trait]
impl RepoBindingRepository for SqliteRepoBindingRepository {
    async fn save(&self, b: &RepoBinding) -> PortResult<()> {
        // Use BEGIN IMMEDIATE so the writer grabs its lock up front instead of
        // upgrading from a deferred read transaction — that upgrade is the
        // classic source of mid-flight SQLITE_BUSY errors.
        let mut tx = self
            .db
            .writes
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_sqlx_err)?;

        sqlx::query(
            r#"
            INSERT INTO repos (id, workspace_id, remote_url, canonical_url, tracked_branch, name, aliases, prefix, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                remote_url = excluded.remote_url,
                canonical_url = excluded.canonical_url,
                tracked_branch = excluded.tracked_branch,
                name = excluded.name,
                aliases = excluded.aliases,
                prefix = excluded.prefix,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(b.id.to_string())
        .bind(b.workspace_id.to_string())
        .bind(&b.remote_url)
        .bind(&b.canonical_url)
        .bind(b.tracked_branch.as_deref())
        .bind(&b.name)
        .bind(serde_json::to_string(&b.aliases).unwrap_or_else(|_| "[]".to_string()))
        .bind(&b.prefix)
        .bind(b.created_at.into_inner())
        .bind(b.updated_at.into_inner())
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_err)?;

        sqlx::query("DELETE FROM worktree_links WHERE repo_id = ?")
            .bind(b.id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;

        for w in &b.worktrees {
            sqlx::query(
                r#"
                INSERT INTO worktree_links (repo_id, path, branch, status, last_seen_at)
                VALUES (?, ?, ?, ?, ?)
                "#,
            )
            .bind(b.id.to_string())
            .bind(w.path.display().to_string())
            .bind(w.branch.as_deref())
            .bind(enum_to_str(&w.status)?)
            .bind(w.last_seen_at.into_inner())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;
        }

        tx.commit().await.map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn get(&self, id: RepoId) -> PortResult<RepoBinding> {
        let row = sqlx::query("SELECT * FROM repos WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.db.reads)
            .await
            .map_err(map_sqlx_err)?
            .ok_or_else(|| PortError::NotFound(format!("repo {id}")))?;
        let mut binding = row_to_binding(&row)?;
        binding.worktrees = load_worktrees(&self.db.reads, id).await?;
        Ok(binding)
    }

    async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> PortResult<Vec<RepoBinding>> {
        let rows = sqlx::query("SELECT * FROM repos WHERE workspace_id = ? ORDER BY created_at")
            .bind(workspace_id.to_string())
            .fetch_all(&self.db.reads)
            .await
            .map_err(map_sqlx_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut binding = row_to_binding(row)?;
            binding.worktrees = load_worktrees(&self.db.reads, binding.id).await?;
            out.push(binding);
        }
        Ok(out)
    }

    async fn find_by_canonical_url(
        &self,
        workspace_id: WorkspaceId,
        canonical_url: &str,
    ) -> PortResult<Option<RepoBinding>> {
        let row = sqlx::query("SELECT * FROM repos WHERE workspace_id = ? AND canonical_url = ?")
            .bind(workspace_id.to_string())
            .bind(canonical_url)
            .fetch_optional(&self.db.reads)
            .await
            .map_err(map_sqlx_err)?;
        match row {
            Some(row) => {
                let mut binding = row_to_binding(&row)?;
                binding.worktrees = load_worktrees(&self.db.reads, binding.id).await?;
                Ok(Some(binding))
            }
            None => Ok(None),
        }
    }

    async fn delete(&self, id: RepoId) -> PortResult<()> {
        sqlx::query("DELETE FROM repos WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.db.writes)
            .await
            .map_err(map_sqlx_err)?;
        Ok(())
    }
}

fn row_to_binding(row: &sqlx::sqlite::SqliteRow) -> PortResult<RepoBinding> {
    let id_str: String = row.try_get("id").map_err(map_sqlx_err)?;
    let workspace_id_str: String = row.try_get("workspace_id").map_err(map_sqlx_err)?;
    let remote_url: String = row.try_get("remote_url").map_err(map_sqlx_err)?;
    let canonical_url: String = row.try_get("canonical_url").map_err(map_sqlx_err)?;
    let tracked_branch: Option<String> = row.try_get("tracked_branch").map_err(map_sqlx_err)?;
    let name_raw: String = row.try_get("name").map_err(map_sqlx_err)?;
    let name = if name_raw.is_empty() {
        domain_repo::derive_name(&canonical_url)
    } else {
        name_raw
    };
    let prefix: String = row.try_get("prefix").map_err(map_sqlx_err)?;
    let aliases_json: String = row.try_get("aliases").map_err(map_sqlx_err)?;
    // The CHECK constraint on `repos.aliases` enforces JSON array shape
    // at write time, so this branch should be unreachable through our
    // code path. If a row's JSON does fail to decode (external tool
    // bypassing the constraint, future schema evolution leaving
    // partial state), prefer a loud mapping error over silently
    // dropping aliases — the latter would corrupt the in-memory model
    // and propagate as missing-alias bugs at the CLI boundary.
    let aliases: Vec<String> = serde_json::from_str(&aliases_json).map_err(|e| {
        PortError::Backend(format!(
            "repo {id_str}: aliases column has malformed JSON: {e}"
        ))
    })?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(map_sqlx_err)?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at").map_err(map_sqlx_err)?;

    Ok(RepoBinding {
        id: parse_uuid::<RepoId>("repo_id", &id_str)?,
        workspace_id: parse_uuid::<WorkspaceId>("workspace_id", &workspace_id_str)?,
        remote_url,
        canonical_url,
        tracked_branch,
        name,
        aliases,
        prefix,
        worktrees: Vec::new(),
        created_at: Timestamp::from_utc(created_at),
        updated_at: Timestamp::from_utc(updated_at),
    })
}

async fn load_worktrees(pool: &SqlitePool, repo_id: RepoId) -> PortResult<Vec<WorktreeLink>> {
    let rows = sqlx::query("SELECT * FROM worktree_links WHERE repo_id = ? ORDER BY path")
        .bind(repo_id.to_string())
        .fetch_all(pool)
        .await
        .map_err(map_sqlx_err)?;
    rows.iter()
        .map(|row| {
            let path: String = row.try_get("path").map_err(map_sqlx_err)?;
            let branch: Option<String> = row.try_get("branch").map_err(map_sqlx_err)?;
            let status: String = row.try_get("status").map_err(map_sqlx_err)?;
            let last_seen_at: DateTime<Utc> = row.try_get("last_seen_at").map_err(map_sqlx_err)?;
            Ok(WorktreeLink {
                path: PathBuf::from(path),
                branch,
                status: enum_from_str::<LinkStatus>("worktree status", &status)?,
                last_seen_at: Timestamp::from_utc(last_seen_at),
            })
        })
        .collect()
}
