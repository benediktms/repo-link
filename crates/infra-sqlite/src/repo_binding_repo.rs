use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use domain_core::{RepoInstanceId, RepoOriginId, Timestamp, WorkspaceId};
use domain_repo::{LinkStatus, RepoBindingView, RepoInstance, RepoOrigin, WorktreeLink};
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
// current column set as of the latest migration. The `schema_const_consistency`
// test in `lib.rs` enforces that against `PRAGMA table_info`, so a future
// migration that forgets to update a const fails in CI rather than silently
// dropping the new column from every read.
// Bare instance column set, used only by the `schema_const_consistency` test
// (and the stale-statement regression test) — runtime reads project the
// `_QUALIFIED` variant below for the origin JOIN, so this is `cfg(test)` to
// avoid a dead-code warning in release builds (CI denies warnings).
#[cfg(test)]
pub(crate) const REPO_INSTANCE_COLS: &str =
    "id, workspace_id, canonical_url, tracked_branch, created_at, updated_at, origin_id";
pub(crate) const REPO_ORIGIN_COLS: &str =
    "id, canonical_url, remote_url, prefix, name, aliases, created_at, updated_at";
pub(crate) const WORKTREE_LINK_COLS: &str = "repo_id, path, branch, status, last_seen_at";

#[async_trait]
impl RepoBindingRepository for SqliteRepoBindingRepository {
    async fn save_origin(&self, origin: &RepoOrigin) -> PortResult<()> {
        sqlx::query(
            r#"
            INSERT INTO repo_origins (id, canonical_url, remote_url, prefix, name, aliases, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                canonical_url = excluded.canonical_url,
                remote_url = excluded.remote_url,
                prefix = excluded.prefix,
                name = excluded.name,
                aliases = excluded.aliases,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(origin.id.to_string())
        .bind(&origin.canonical_url)
        .bind(&origin.remote_url)
        .bind(&origin.prefix)
        .bind(&origin.name)
        .bind(serde_json::to_string(&origin.aliases).unwrap_or_else(|_| "[]".to_string()))
        .bind(origin.created_at.into_inner())
        .bind(origin.updated_at.into_inner())
        .execute(&self.db.writes)
        .await
        .map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn save_instance(&self, instance: &RepoInstance) -> PortResult<()> {
        let mut tx = self
            .db
            .writes
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_sqlx_err)?;

        sqlx::query(
            r#"
            INSERT INTO repo_instances (id, workspace_id, canonical_url, tracked_branch, origin_id, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                workspace_id = excluded.workspace_id,
                canonical_url = excluded.canonical_url,
                tracked_branch = excluded.tracked_branch,
                origin_id = excluded.origin_id,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(instance.id.to_string())
        .bind(instance.workspace_id.to_string())
        .bind(&instance.canonical_url)
        .bind(instance.tracked_branch.as_deref())
        .bind(instance.origin_id.to_string())
        .bind(instance.created_at.into_inner())
        .bind(instance.updated_at.into_inner())
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_err)?;

        sqlx::query("DELETE FROM worktree_links WHERE repo_id = ?")
            .bind(instance.id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;

        for w in &instance.worktrees {
            sqlx::query(
                r#"
                INSERT INTO worktree_links (repo_id, path, branch, status, last_seen_at)
                VALUES (?, ?, ?, ?, ?)
                "#,
            )
            .bind(instance.id.to_string())
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

    async fn get(&self, id: RepoInstanceId) -> PortResult<RepoBindingView> {
        let row = sqlx::query(&format!(
            "SELECT {REPO_INSTANCE_COLS_QUALIFIED}, {REPO_ORIGIN_COLS_QUALIFIED} \
             FROM repo_instances ri \
             JOIN repo_origins ro ON ro.id = ri.origin_id \
             WHERE ri.id = ?"
        ))
        .bind(id.to_string())
        .fetch_optional(&self.db.reads)
        .await
        .map_err(map_sqlx_err)?
        .ok_or_else(|| PortError::NotFound(format!("repo instance {id}")))?;
        let instance = row_to_instance(&row)?;
        let mut view = RepoBindingView {
            origin: row_to_origin(&row)?,
            instance,
        };
        view.instance.worktrees = load_worktrees(&self.db.reads, id).await?;
        Ok(view)
    }

    async fn get_origin(&self, id: RepoOriginId) -> PortResult<RepoOrigin> {
        let row = sqlx::query(&format!(
            "SELECT {REPO_ORIGIN_COLS} FROM repo_origins WHERE id = ?"
        ))
        .bind(id.to_string())
        .fetch_optional(&self.db.reads)
        .await
        .map_err(map_sqlx_err)?
        .ok_or_else(|| PortError::NotFound(format!("repo origin {id}")))?;
        row_to_origin(&row)
    }

    async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> PortResult<Vec<RepoBindingView>> {
        let rows = sqlx::query(&format!(
            "SELECT {REPO_INSTANCE_COLS_QUALIFIED}, {REPO_ORIGIN_COLS_QUALIFIED} \
             FROM repo_instances ri \
             JOIN repo_origins ro ON ro.id = ri.origin_id \
             WHERE ri.workspace_id = ? ORDER BY ri.created_at"
        ))
        .bind(workspace_id.to_string())
        .fetch_all(&self.db.reads)
        .await
        .map_err(map_sqlx_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let instance = row_to_instance(row)?;
            let instance_id = instance.id;
            let mut view = RepoBindingView {
                origin: row_to_origin(row)?,
                instance,
            };
            view.instance.worktrees = load_worktrees(&self.db.reads, instance_id).await?;
            out.push(view);
        }
        Ok(out)
    }

    async fn find_by_canonical_url(
        &self,
        workspace_id: WorkspaceId,
        canonical_url: &str,
    ) -> PortResult<Option<RepoBindingView>> {
        let row = sqlx::query(&format!(
            "SELECT {REPO_INSTANCE_COLS_QUALIFIED}, {REPO_ORIGIN_COLS_QUALIFIED} \
             FROM repo_instances ri \
             JOIN repo_origins ro ON ro.id = ri.origin_id \
             WHERE ri.workspace_id = ? AND ri.canonical_url = ?"
        ))
        .bind(workspace_id.to_string())
        .bind(canonical_url)
        .fetch_optional(&self.db.reads)
        .await
        .map_err(map_sqlx_err)?;
        match row {
            Some(row) => {
                let instance = row_to_instance(&row)?;
                let instance_id = instance.id;
                let mut view = RepoBindingView {
                    origin: row_to_origin(&row)?,
                    instance,
                };
                view.instance.worktrees = load_worktrees(&self.db.reads, instance_id).await?;
                Ok(Some(view))
            }
            None => Ok(None),
        }
    }

    async fn find_origin_by_canonical_url(
        &self,
        canonical_url: &str,
    ) -> PortResult<Option<RepoOrigin>> {
        let row = sqlx::query(&format!(
            "SELECT {REPO_ORIGIN_COLS} FROM repo_origins WHERE canonical_url = ?"
        ))
        .bind(canonical_url)
        .fetch_optional(&self.db.reads)
        .await
        .map_err(map_sqlx_err)?;
        match row {
            Some(row) => Ok(Some(row_to_origin(&row)?)),
            None => Ok(None),
        }
    }

    async fn find_origin_by_prefix(&self, prefix: &str) -> PortResult<Option<RepoOrigin>> {
        // Empty prefix is the unset-sentinel; reject explicitly so a
        // bug elsewhere doesn't accidentally return "any unbackfilled
        // row" via a `WHERE prefix = ''` match.
        if prefix.is_empty() {
            return Ok(None);
        }
        let row = sqlx::query(&format!(
            "SELECT {REPO_ORIGIN_COLS} FROM repo_origins WHERE prefix = ?"
        ))
        .bind(prefix)
        .fetch_optional(&self.db.reads)
        .await
        .map_err(map_sqlx_err)?;
        match row {
            Some(row) => Ok(Some(row_to_origin(&row)?)),
            None => Ok(None),
        }
    }

    async fn find_by_remote_mapping(
        &self,
        workspace_id: WorkspaceId,
        provider: &str,
        remote_id: &str,
    ) -> PortResult<Option<RepoOriginId>> {
        // RFC 0005 §D4: `remote_mappings.filing_repo_id` is now in ORIGIN id
        // space. JOIN against `repo_instances` filtered by `workspace_id` to:
        //   - prevent cross-workspace import ambiguity, AND
        //   - filter out rows whose origin references no instance in this workspace
        //     (silent-divergence protection — the doctor must never re-point a task
        //     to a dead origin). LIMIT 2: 0 = no match, 1 = unambiguous, 2+ =
        //     ambiguous → return None so the user must pick with `--target`.
        let rows = sqlx::query(
            "SELECT rm.filing_repo_id \
             FROM remote_mappings rm \
             JOIN repo_instances r ON r.origin_id = rm.filing_repo_id \
             WHERE r.workspace_id = ? AND rm.provider = ? AND rm.remote_id = ? \
             LIMIT 2",
        )
        .bind(workspace_id.to_string())
        .bind(provider)
        .bind(remote_id)
        .fetch_all(&self.db.reads)
        .await
        .map_err(map_sqlx_err)?;
        match rows.as_slice() {
            [row] => {
                let id_str: String = row.try_get("filing_repo_id").map_err(map_sqlx_err)?;
                let id: RepoOriginId = id_str.parse().map_err(|e: domain_core::IdParseError| {
                    PortError::Backend(format!("remote_mappings.filing_repo_id is malformed: {e}"))
                })?;
                Ok(Some(id))
            }
            // Zero rows (no match) OR ≥2 rows (ambiguous) both collapse to `None`
            // so the doctor surfaces the situation as `unresolved`.
            _ => Ok(None),
        }
    }

    async fn delete(&self, id: RepoInstanceId) -> PortResult<()> {
        sqlx::query("DELETE FROM repo_instances WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.db.writes)
            .await
            .map_err(map_sqlx_err)?;
        Ok(())
    }
}

// Qualified instance column list for JOINs: prefixed with `ri.` so the bare
// columns shared with `repo_origins` (`id`, `canonical_url`, `created_at`,
// `updated_at`) are not ambiguous. `ri.id` yields a result column still named
// `id`, so `row_to_instance` reads it by the same bare name.
const REPO_INSTANCE_COLS_QUALIFIED: &str = "ri.id, ri.workspace_id, ri.canonical_url, ri.tracked_branch, ri.created_at, ri.updated_at, ri.origin_id";

// Qualified origin column list for JOINs: prefixed with `ro.` so they don't
// collide with instance columns (both tables have `id`, `canonical_url`,
// `created_at`, `updated_at`). `row_to_origin` reads the `ro_`-prefixed aliases.
const REPO_ORIGIN_COLS_QUALIFIED: &str = "ro.id AS ro_id, ro.canonical_url AS ro_canonical_url, ro.remote_url AS ro_remote_url, \
     ro.prefix AS ro_prefix, ro.name AS ro_name, ro.aliases AS ro_aliases, \
     ro.created_at AS ro_created_at, ro.updated_at AS ro_updated_at";

fn row_to_instance(row: &sqlx::sqlite::SqliteRow) -> PortResult<RepoInstance> {
    let id_str: String = row.try_get("id").map_err(map_sqlx_err)?;
    let workspace_id_str: String = row.try_get("workspace_id").map_err(map_sqlx_err)?;
    let canonical_url: String = row.try_get("canonical_url").map_err(map_sqlx_err)?;
    let tracked_branch: Option<String> = row.try_get("tracked_branch").map_err(map_sqlx_err)?;
    let origin_id_str: String = row.try_get("origin_id").map_err(map_sqlx_err)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(map_sqlx_err)?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at").map_err(map_sqlx_err)?;

    Ok(RepoInstance {
        id: parse_uuid::<RepoInstanceId>("repo_instance_id", &id_str)?,
        workspace_id: parse_uuid::<WorkspaceId>("workspace_id", &workspace_id_str)?,
        origin_id: parse_uuid::<RepoOriginId>("origin_id", &origin_id_str)?,
        canonical_url,
        tracked_branch,
        worktrees: Vec::new(),
        created_at: Timestamp::from_utc(created_at),
        updated_at: Timestamp::from_utc(updated_at),
    })
}

fn row_to_origin(row: &sqlx::sqlite::SqliteRow) -> PortResult<RepoOrigin> {
    // Try qualified column names first (JOIN queries), fall back to bare names
    // (direct SELECT from repo_origins).
    let id_str: String = row
        .try_get("ro_id")
        .or_else(|_| row.try_get("id"))
        .map_err(map_sqlx_err)?;
    let canonical_url: String = row
        .try_get("ro_canonical_url")
        .or_else(|_| row.try_get("canonical_url"))
        .map_err(map_sqlx_err)?;
    let remote_url: String = row
        .try_get("ro_remote_url")
        .or_else(|_| row.try_get("remote_url"))
        .map_err(map_sqlx_err)?;
    let prefix: String = row
        .try_get("ro_prefix")
        .or_else(|_| row.try_get("prefix"))
        .map_err(map_sqlx_err)?;
    let name_raw: String = row
        .try_get("ro_name")
        .or_else(|_| row.try_get("name"))
        .map_err(map_sqlx_err)?;
    let name = if name_raw.is_empty() {
        domain_repo::derive_name(&canonical_url)
    } else {
        name_raw
    };
    let aliases_json: String = row
        .try_get("ro_aliases")
        .or_else(|_| row.try_get("aliases"))
        .map_err(map_sqlx_err)?;
    let aliases: Vec<String> = serde_json::from_str(&aliases_json).map_err(|e| {
        PortError::Backend(format!(
            "repo origin {id_str}: aliases column has malformed JSON: {e}"
        ))
    })?;
    let created_at: DateTime<Utc> = row
        .try_get("ro_created_at")
        .or_else(|_| row.try_get("created_at"))
        .map_err(map_sqlx_err)?;
    let updated_at: DateTime<Utc> = row
        .try_get("ro_updated_at")
        .or_else(|_| row.try_get("updated_at"))
        .map_err(map_sqlx_err)?;

    Ok(RepoOrigin {
        id: parse_uuid::<RepoOriginId>("repo_origin_id", &id_str)?,
        canonical_url,
        remote_url,
        prefix,
        name,
        aliases,
        created_at: Timestamp::from_utc(created_at),
        updated_at: Timestamp::from_utc(updated_at),
    })
}

async fn load_worktrees(
    pool: &SqlitePool,
    repo_id: RepoInstanceId,
) -> PortResult<Vec<WorktreeLink>> {
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
    //! The fix pins the projection to [`REPO_INSTANCE_COLS`], so `column_count()`
    //! stays constant regardless of any future ADD COLUMN. This test reproduces
    //! the cross-process race in-process and proves the fixed projection survives.

    use std::str::FromStr;

    use sqlx::Row;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use tempfile::TempDir;

    use super::REPO_INSTANCE_COLS;

    #[tokio::test]
    async fn repo_instance_cols_survives_cross_connection_add_column() {
        // (1) Fresh migrated DB in a TempDir; insert a workspace, an origin,
        // then an instance row (the FKs require the parents to exist first).
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
            "INSERT INTO repo_origins \
             (id, canonical_url, remote_url, prefix, name, aliases, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, '[]', ?, ?)",
        )
        .bind("33333333-3333-3333-3333-333333333333")
        .bind("github.com/o/r")
        .bind("git@github.com:o/r.git")
        .bind("rr")
        .bind("r")
        .bind(now)
        .bind(now)
        .execute(&db.writes)
        .await
        .expect("insert repo_origin");

        sqlx::query(
            "INSERT INTO repo_instances \
             (id, workspace_id, canonical_url, tracked_branch, origin_id, created_at, updated_at) \
             VALUES (?, ?, ?, NULL, ?, ?, ?)",
        )
        .bind("22222222-2222-2222-2222-222222222222")
        .bind("11111111-1111-1111-1111-111111111111")
        .bind("github.com/o/r")
        .bind("33333333-3333-3333-3333-333333333333")
        .bind(now)
        .bind(now)
        .execute(&db.writes)
        .await
        .expect("insert repo_instance");

        let url = format!("sqlite://{}", db_path.display());

        // (2) Dedicated connection with statement caching ON (sqlx default).
        let cached = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::from_str(&url).expect("connect opts"))
            .await
            .expect("dedicated cached connection");

        // (3) Run the SELECT once to cache the prepared statement + its column metadata.
        let before = sqlx::query(&format!("SELECT {REPO_INSTANCE_COLS} FROM repo_instances"))
            .fetch_all(&cached)
            .await
            .expect("initial select caches the statement");
        assert_eq!(before.len(), 1, "expected the one seeded instance row");

        // (4) Through a SEPARATE connection (cross-process simulation), add a column.
        let other = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::from_str(&url).expect("connect opts"))
            .await
            .expect("separate ALTER connection");
        sqlx::query("ALTER TABLE repo_instances ADD COLUMN regression_probe_110 TEXT")
            .execute(&other)
            .await
            .expect("add column from a separate connection");

        // (5) Re-run the SAME SELECT on the cached connection. Pinned projection
        // means column_count() stays constant → no panic.
        let after = sqlx::query(&format!("SELECT {REPO_INSTANCE_COLS} FROM repo_instances"))
            .fetch_all(&cached)
            .await
            .expect("select after cross-connection ADD COLUMN must not panic");
        assert_eq!(after.len(), 1, "row count unchanged after ADD COLUMN");

        let id: String = after[0].try_get("id").expect("id decodes");
        assert_eq!(id, "22222222-2222-2222-2222-222222222222");
    }
}
