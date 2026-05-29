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

// Explicit, fixed column lists pin `column_count()` to a constant so the
// long-lived read pool's cached prepared statements survive a concurrent
// `ALTER TABLE ... ADD COLUMN` from another process (#110). A bare `SELECT *`
// caches the column metadata at prepare time but re-reads the live column
// count from the re-prepared bytecode at row-decode time; after an ADD COLUMN
// the live count outruns the cached vec and `columns[len]` panics on a sqlx
// worker, crashing the daemon. Decoding is by name (`row.try_get`), so order
// is irrelevant — only completeness matters. Each const is the table's full
// current column set as of the latest migration.
const REPO_COLS: &str = "id, workspace_id, remote_url, canonical_url, tracked_branch, created_at, updated_at, name, aliases, prefix";
const WORKTREE_LINK_COLS: &str = "repo_id, path, branch, status, last_seen_at";

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
        let row = sqlx::query(&format!("SELECT {REPO_COLS} FROM repos WHERE id = ?"))
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
        let rows = sqlx::query(&format!(
            "SELECT {REPO_COLS} FROM repos WHERE workspace_id = ? ORDER BY created_at"
        ))
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
        let row = sqlx::query(&format!(
            "SELECT {REPO_COLS} FROM repos WHERE workspace_id = ? AND canonical_url = ?"
        ))
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

    async fn find_by_prefix(&self, prefix: &str) -> PortResult<Option<RepoBinding>> {
        // Empty prefix is the unset-sentinel; reject explicitly so a
        // bug elsewhere doesn't accidentally return "any unbacklfilled
        // row" via a `WHERE prefix = ''` match.
        if prefix.is_empty() {
            return Ok(None);
        }
        let row = sqlx::query(&format!("SELECT {REPO_COLS} FROM repos WHERE prefix = ?"))
            .bind(prefix)
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
    let rows = sqlx::query(&format!(
        "SELECT {WORKTREE_LINK_COLS} FROM worktree_links WHERE repo_id = ? ORDER BY path"
    ))
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

#[cfg(test)]
mod stale_statement_regression {
    //! Regression for the #110 daemon crash: a long-lived sqlx-sqlite read
    //! pool caches a prepared statement's column *metadata* at prepare time,
    //! but `SqliteRow` re-reads the live `column_count()` from the re-prepared
    //! bytecode at decode time. When another process runs
    //! `ALTER TABLE ... ADD COLUMN` on the shared DB file, the live count
    //! outruns the cached column vec and `columns[len]` panics
    //! ("index out of bounds") on a sqlx worker thread, crashing the tick.
    //!
    //! The fix pins the projection to [`REPO_COLS`], so `column_count()` stays
    //! constant regardless of any future ADD COLUMN. This test reproduces the
    //! cross-process race in-process and proves the fixed projection survives:
    //!
    //! - The cached side uses a **dedicated** connection with statement
    //!   caching left **ON** (sqlx's default). We do NOT reuse the production
    //!   read pool, because a separate defense-in-depth change disables the
    //!   cache there — that would mask the bug. Keeping caching on here is what
    //!   gives the test teeth: it exercises the exact path that panicked.
    //! - The `ALTER TABLE` runs through a **separate** connection to the same
    //!   file, simulating the other process: it must not invalidate the cached
    //!   side's prepared statement, just as a cross-process DDL cannot.
    //!
    //! With the bare `SELECT *` this file used to issue, step (5) would panic;
    //! pinned to `REPO_COLS` it returns the row cleanly. The test uses the real
    //! private `REPO_COLS` const directly, so reverting the projection to `*`
    //! re-breaks it.

    use std::str::FromStr;

    use sqlx::Row;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use tempfile::TempDir;

    use super::REPO_COLS;

    #[tokio::test]
    async fn repo_cols_survives_cross_connection_add_column() {
        // (1) Fresh migrated DB in a TempDir; insert a workspace then a repo
        // row (the repos.workspace_id FK requires the parent to exist first).
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("repo-link.db");
        let db = crate::open_from_path(&db_path).await.expect("open db");

        let now = "2026-05-30T00:00:00Z";
        sqlx::query(
            "INSERT INTO workspaces (id, name, status, local_only, created_at, updated_at) \
             VALUES (?, ?, 'active', 1, ?, ?)",
        )
        .bind("11111111-1111-1111-1111-111111111111")
        .bind("regression-ws")
        .bind(now)
        .bind(now)
        .execute(&db.writes)
        .await
        .expect("insert workspace");

        sqlx::query(
            "INSERT INTO repos \
             (id, workspace_id, remote_url, canonical_url, tracked_branch, created_at, updated_at, name, aliases, prefix) \
             VALUES (?, ?, ?, ?, NULL, ?, ?, ?, '[]', ?)",
        )
        .bind("22222222-2222-2222-2222-222222222222")
        .bind("11111111-1111-1111-1111-111111111111")
        .bind("git@github.com:o/r.git")
        .bind("github.com/o/r")
        .bind(now)
        .bind(now)
        .bind("r")
        .bind("rr")
        .execute(&db.writes)
        .await
        .expect("insert repo");

        let url = format!("sqlite://{}", db_path.display());

        // (2) Dedicated connection with statement caching ON (sqlx default),
        // independent of the production read pool's cache setting.
        let cached = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::from_str(&url).expect("connect opts"))
            .await
            .expect("dedicated cached connection");

        // (3) Run the SELECT once to cache the prepared statement + its column
        // metadata on the dedicated connection.
        let before = sqlx::query(&format!("SELECT {REPO_COLS} FROM repos"))
            .fetch_all(&cached)
            .await
            .expect("initial select caches the statement");
        assert_eq!(before.len(), 1, "expected the one seeded repo row");

        // (4) Through a SEPARATE connection to the same file (cross-process
        // simulation), add a column. This re-shapes the table bytecode that
        // the cached connection's statement will re-prepare against.
        let other = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::from_str(&url).expect("connect opts"))
            .await
            .expect("separate ALTER connection");
        sqlx::query("ALTER TABLE repos ADD COLUMN regression_probe_110 TEXT")
            .execute(&other)
            .await
            .expect("add column from a separate connection");

        // (5) Re-run the SAME SELECT on the cached connection. With a fixed
        // projection the column count is pinned, so this is Ok; a bare
        // `SELECT *` would panic here with index-out-of-bounds.
        let after = sqlx::query(&format!("SELECT {REPO_COLS} FROM repos"))
            .fetch_all(&cached)
            .await
            .expect("select after cross-connection ADD COLUMN must not panic");
        assert_eq!(after.len(), 1, "row count unchanged after ADD COLUMN");

        // Decoding still works by name on the pinned columns.
        let id: String = after[0].try_get("id").expect("id decodes");
        assert_eq!(id, "22222222-2222-2222-2222-222222222222");
    }
}
