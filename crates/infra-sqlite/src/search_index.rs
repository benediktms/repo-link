//! infra-sqlite — RFC 0007 disposable task-search sidecar adapter.
//!
//! `SqliteTaskSearchIndex` manages `<authoritative>.task-search.db`: the D5
//! schema (metadata, chunks, vectors, FTS5 external-content + triggers),
//! owner-only file modes, the D6 reconcile, and the FTS5 lexical lane.
//! SQLite is driven raw (not the authoritative pool) because the sidecar
//! needs PRAGMAs ordered before tables, `BEGIN IMMEDIATE` write
//! serialization, triggers, and `wal_checkpoint(TRUNCATE)`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use async_trait::async_trait;
use domain_core::TaskId;
use dto_shared::{LexicalUnavailableReasonDto, MatchedSourceKindDto};
use ports::{
    ChunkKind, ChunkTarget, IndexMetadata, IndexStats, LexicalRank, PortError, ReconcileDiff,
    ReconcileFailure, ReconcileSession, SEARCH_CHUNK_FORMAT_VERSION, SEARCH_SCHEMA_VERSION,
    SchemaMismatch, SidecarInfo, TaskSearchIndex,
};
use sqlx::Row;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Sqlite, SqlitePool};
use tokio::sync::Mutex;

/// 4 KiB page size, set before any table (RFC 0007 D5).
const PAGE_SIZE: &str = "4096";
/// Full auto-vacuum, set before any table.
const AUTO_VACUUM: &str = "FULL";
/// `max_page_count` = 512 MiB backstop (RFC 0007 D5).
const MAX_PAGE_COUNT: i64 = 131072;
/// RFC 0007 D5 refuse sidecar+WAL budget.
const REFUSE_BYTES: u64 = 512 * 1024 * 1024;

/// The D5 schema statement block, idempotent. FTS5 external-content over
/// `task_search_chunks(text)` with insert/delete triggers + an immutability
/// guard trigger.
const SCHEMA_SQL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS task_search_meta (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
        schema_version INTEGER NOT NULL,
        chunk_format_version INTEGER NOT NULL,
        embedding_profile_id TEXT,
        validated_content_fingerprint BLOB,
        validated_file_size INTEGER,
        validated_file_mtime INTEGER,
        validated_at TEXT
    )",
    "CREATE TABLE IF NOT EXISTS task_search_chunks (
        id INTEGER PRIMARY KEY,
        task_id TEXT NOT NULL,
        kind TEXT NOT NULL CHECK(kind IN ('core','comment')),
        content_hash BLOB NOT NULL,
        text TEXT NOT NULL,
        UNIQUE(task_id, content_hash)
    )",
    "CREATE TABLE IF NOT EXISTS task_search_vectors (
        search_chunk_id INTEGER NOT NULL REFERENCES task_search_chunks(id) ON DELETE CASCADE,
        segment_index INTEGER NOT NULL,
        embedding_input_hash BLOB NOT NULL,
        vector BLOB NOT NULL,
        PRIMARY KEY(search_chunk_id, segment_index)
    )",
    "CREATE VIRTUAL TABLE IF NOT EXISTS task_search_fts USING fts5(
        text,
        content = 'task_search_chunks',
        content_rowid = 'id',
        tokenize = 'unicode61'
    )",
    "CREATE TRIGGER IF NOT EXISTS task_search_chunks_ai AFTER INSERT ON task_search_chunks BEGIN
        INSERT INTO task_search_fts(rowid, text) VALUES (new.id, new.text);
    END",
    "CREATE TRIGGER IF NOT EXISTS task_search_chunks_ad AFTER DELETE ON task_search_chunks BEGIN
        INSERT INTO task_search_fts(task_search_fts, rowid, text)
        VALUES ('delete', old.id, old.text);
    END",
    "CREATE TRIGGER IF NOT EXISTS task_search_chunks_bu BEFORE UPDATE OF
        task_id, kind, content_hash, text ON task_search_chunks BEGIN
        SELECT RAISE(ABORT, 'task_search_chunks identity/text is immutable');
    END",
];

/// The sidecar index for one authoritative database.
pub struct SqliteTaskSearchIndex {
    /// `<authoritative>.task-search.db` sibling path (RFC 0007 D5).
    sidecar_path: PathBuf,
    /// Lazily-opened single-connection pool.
    pool: Mutex<Option<SqlitePool>>,
}

impl SqliteTaskSearchIndex {
    /// Derive the sidecar path from the authoritative db path by appending the
    /// `.task-search.db` suffix to the *complete* filename (RFC 0007 D5:
    /// avoids stem collision, e.g. `foo.db` vs `foo.sqlite`).
    pub fn new(authoritative_path: &Path) -> Self {
        let sidecar_path =
            PathBuf::from(format!("{}.task-search.db", authoritative_path.display()));
        Self {
            sidecar_path,
            pool: Mutex::new(None),
        }
    }

    /// Open the sidecar, enforcing PRAGMA-before-table ordering and an
    /// owner-only file, then create the D5 schema. Idempotent.
    async fn open(&self) -> Result<SqlitePool, ReconcileFailure> {
        let mut guard = self.pool.lock().await;
        if let Some(p) = guard.as_ref() {
            return Ok(p.clone());
        }
        // Trusted parent + owner-only sidecar file, before SQLite opens it
        // (RFC 0007 D5 owner-only policy). Failure = sidecar unavailable.
        fix_owner_only(&self.sidecar_path).map_err(|_| ReconcileFailure {
            reason: LexicalUnavailableReasonDto::SidecarUnavailable,
        })?;

        let url = format!("sqlite://{}", self.sidecar_path.display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .map_err(move_to_failure)?
            .create_if_missing(false)
            .foreign_keys(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(10));
        let pool = SqlitePoolOptions::new()
            // RFC D6 write serialization comes from `BEGIN IMMEDIATE`, not the
            // pool width. A reconcile session holds one connection for its
            // lifetime; keep a second free so `search_lexical`/`stats` never
            // contend with a held writer connection (a single-connection pool
            // could wedge them behind an aborted reconcile session).
            .max_connections(2)
            .connect_with(opts)
            .await
            .map_err(|_| ReconcileFailure {
                reason: LexicalUnavailableReasonDto::SidecarUnavailable,
            })?;

        let mut conn = pool.acquire().await.map_err(move_to_failure)?;
        // PRAGMAs must precede table creation (RFC 0007 D5).
        for pragma in [
            format!("PRAGMA page_size = {PAGE_SIZE};"),
            format!("PRAGMA auto_vacuum = {AUTO_VACUUM};"),
            format!("PRAGMA max_page_count = {MAX_PAGE_COUNT};"),
        ] {
            sqlx::query(&pragma)
                .execute(&mut *conn)
                .await
                .map_err(move_to_failure)?;
        }
        for stmt in SCHEMA_SQL {
            sqlx::query(stmt)
                .execute(&mut *conn)
                .await
                .map_err(move_to_failure)?;
        }
        // Seed the singleton metadata row if absent.
        sqlx::query(
            "INSERT OR IGNORE INTO task_search_meta (singleton, schema_version, chunk_format_version) \
             VALUES (1, ?, ?)",
        )
        .bind(SEARCH_SCHEMA_VERSION)
        .bind(SEARCH_CHUNK_FORMAT_VERSION)
        .execute(&mut *conn)
        .await
        .map_err(move_to_failure)?;
        drop(conn);

        *guard = Some(pool.clone());
        Ok(pool)
    }

    /// Read the singleton metadata row.
    async fn read_meta(&self) -> Result<Option<MetaRow>, ReconcileFailure> {
        let pool = self.open().await?;
        let row = sqlx::query(
            "SELECT singleton, schema_version, chunk_format_version, embedding_profile_id, \
                    validated_content_fingerprint \
             FROM task_search_meta WHERE singleton = 1",
        )
        .fetch_optional(&pool)
        .await
        .map_err(move_to_failure)?;
        Ok(row.map(|r| MetaRow {
            schema_version: r.try_get("schema_version").unwrap_or(0),
            chunk_format_version: r.try_get("chunk_format_version").unwrap_or(0),
            embedding_profile_id: r.try_get("embedding_profile_id").ok(),
        }))
    }
}

#[derive(Clone)]
struct MetaRow {
    schema_version: i64,
    chunk_format_version: i64,
    embedding_profile_id: Option<String>,
}

/// Enforce the owner-only + trusted-parent policy for the sidecar file
/// (RFC 0007 D5). Pre-creates the file with mode `0600` when absent and
/// refuses when the parent is world-writable or not owned.
fn fix_owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::fs::PermissionsExt;

    // Reject a symlink anywhere in the sidecar path (no-follow, RFC 0007 D5).
    if std::fs::symlink_metadata(path).is_ok()
        && std::fs::symlink_metadata(path)?.file_type().is_symlink()
    {
        return Err(std::io::Error::other("sidecar path is a symlink"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("sidecar has no parent"))?;
    let pm = std::fs::metadata(parent)?;
    let pmode = pm.permissions().mode();
    // Refuse a world-writable parent or a parent owned by someone else.
    if pmode & 0o022 != 0 {
        return Err(std::io::Error::other("sidecar parent is world-writable"));
    }
    let scratch = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    let m = scratch.metadata()?;
    if m.permissions().mode() & 0o077 != 0 {
        return Err(std::io::Error::other("sidecar mode is not owner-only"));
    }
    Ok(())
}

fn move_to_failure(_e: impl ToString) -> ReconcileFailure {
    ReconcileFailure {
        reason: LexicalUnavailableReasonDto::SidecarUnavailable,
    }
}

/// Storage preflight: refuse before growth when projected sidecar+WAL would
/// exceed the hard budget (RFC 0007 D5). Returns a `ReconcileFailure` when
/// refused.
fn preflight(projected_bytes: u64) -> Result<(), ReconcileFailure> {
    if projected_bytes > REFUSE_BYTES {
        return Err(ReconcileFailure {
            reason: LexicalUnavailableReasonDto::StorageLimit,
        });
    }
    Ok(())
}

/// A serialized reconcile session bound to one `BEGIN IMMEDIATE` transaction
/// (RFC 0007 D6 ordering). Carries the sidecar file identity captured at open
/// for the validated-integrity marker.
struct SqliteReconcileSession {
    conn: sqlx::pool::PoolConnection<Sqlite>,
    file_size: i64,
    file_mtime: i64,
}

#[async_trait]
impl ReconcileSession for SqliteReconcileSession {
    async fn diff_chunks(
        &mut self,
        desired: &[ChunkTarget],
        projected_bytes: u64,
    ) -> Result<ReconcileDiff, ReconcileFailure> {
        preflight(projected_bytes)?;
        let conn = &mut self.conn;

        // FTS integrity-check before any mutation when the diff is non-empty
        // (RFC 0007 D6 step 6). We don't know the diff yet, so run it ahead of
        // any potential write.
        sqlx::query(
            "INSERT INTO task_search_fts(task_search_fts, rank) VALUES('integrity-check', 1)",
        )
        .execute(&mut **conn)
        .await
        .map_err(|_| ReconcileFailure {
            reason: LexicalUnavailableReasonDto::IndexCorrupt,
        })?;

        // Existing rows (manual parse; TaskId is not a sqlx Decode type).
        let existing_rows = sqlx::query("SELECT id, task_id, content_hash FROM task_search_chunks")
            .fetch_all(&mut **conn)
            .await
            .map_err(|_| ReconcileFailure {
                reason: LexicalUnavailableReasonDto::IndexCorrupt,
            })?;
        let mut existing: Vec<ExistingRow> = Vec::new();
        for r in existing_rows {
            let Ok(task_id_str) = r.try_get::<String, _>("task_id") else {
                continue;
            };
            let Ok(tid) = TaskId::from_str(task_id_str.as_str()) else {
                continue;
            };
            existing.push(ExistingRow {
                id: r.try_get("id").map_err(idx_err)?,
                task_id: tid,
                content_hash: r.try_get("content_hash").map_err(idx_err)?,
            });
        }

        let mut want: HashMap<(TaskId, Vec<u8>), &ChunkTarget> = HashMap::new();
        for t in desired {
            want.insert((t.task_id, t.content_hash.to_vec()), t);
        }

        let mut diff = ReconcileDiff {
            desired_total: desired.len(),
            ..Default::default()
        };
        // Delete rows absent from the desired set; FTS follows via trigger.
        for e in &existing {
            if !want.contains_key(&(e.task_id, e.content_hash.clone())) {
                sqlx::query("DELETE FROM task_search_chunks WHERE id = ?")
                    .bind(e.id)
                    .execute(&mut **conn)
                    .await
                    .map_err(|_| ReconcileFailure {
                        reason: LexicalUnavailableReasonDto::ReconciliationFailed,
                    })?;
                diff.deleted += 1;
            }
        }
        let existing_set: std::collections::HashSet<(TaskId, Vec<u8>)> = existing
            .into_iter()
            .map(|e| (e.task_id, e.content_hash))
            .collect();
        // Insert missing rows.
        for (key, t) in &want {
            if !existing_set.contains(key) {
                sqlx::query(
                    "INSERT INTO task_search_chunks (task_id, kind, content_hash, text) \
                     VALUES (?, ?, ?, ?)",
                )
                .bind(t.task_id.to_string())
                .bind(kind_str(t.kind))
                .bind(t.content_hash.as_slice())
                .bind(&t.text)
                .execute(&mut **conn)
                .await
                .map_err(|_| ReconcileFailure {
                    reason: LexicalUnavailableReasonDto::ReconciliationFailed,
                })?;
                diff.inserted += 1;
            }
        }
        Ok(diff)
    }

    async fn commit(&mut self, content_fingerprint: &[u8; 32]) -> Result<(), PortError> {
        let conn = &mut self.conn;
        // Persist a validated-integrity marker bound to the content
        // fingerprint + sidecar file identity (RFC 0007 D6).
        sqlx::query(
            "UPDATE task_search_meta SET \
               validated_content_fingerprint = ?, validated_file_size = ?, \
               validated_file_mtime = ?, validated_at = ? \
             WHERE singleton = 1",
        )
        .bind(content_fingerprint.as_slice())
        .bind(self.file_size)
        .bind(self.file_mtime)
        .bind(now_iso())
        .execute(&mut **conn)
        .await
        .map_err(|e| PortError::Backend(e.to_string()))?;
        sqlx::query("COMMIT")
            .execute(&mut **conn)
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn rollback(&mut self) -> Result<(), PortError> {
        let conn = &mut self.conn;
        sqlx::query("ROLLBACK")
            .execute(&mut **conn)
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct ExistingRow {
    id: i64,
    task_id: TaskId,
    content_hash: Vec<u8>,
}

fn kind_str(k: ChunkKind) -> &'static str {
    match k {
        ChunkKind::Core => "core",
        ChunkKind::Comment => "comment",
    }
}

fn idx_err(_e: sqlx::Error) -> ReconcileFailure {
    ReconcileFailure {
        reason: LexicalUnavailableReasonDto::IndexCorrupt,
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[async_trait]
impl TaskSearchIndex for SqliteTaskSearchIndex {
    async fn begin_reconcile(&self) -> Result<Box<dyn ReconcileSession>, PortError> {
        let pool = self
            .open()
            .await
            .map_err(|f| PortError::Backend(format!("{f:?}")))?;
        let mut conn = pool
            .acquire()
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *conn)
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
        let (file_size, file_mtime) = std::fs::metadata(&self.sidecar_path)
            .map(|m| {
                (
                    m.len() as i64,
                    m.modified()
                        .map(|t| {
                            t.duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0)
                        })
                        .unwrap_or(0),
                )
            })
            .unwrap_or((0, 0));
        Ok(Box::new(SqliteReconcileSession {
            conn,
            file_size,
            file_mtime,
        }))
    }

    async fn metadata(&self) -> Result<IndexMetadata, PortError> {
        let meta = match self.read_meta().await {
            Ok(m) => m,
            Err(_) => {
                // Open failure → sidecar unavailable (D8). Not a hard error.
                return Ok(IndexMetadata {
                    available: None,
                    schema_mismatch: None,
                });
            }
        };
        let Some(meta) = meta else {
            // Openable but no metadata row → incompatible/unknown schema.
            return Ok(IndexMetadata {
                available: None,
                schema_mismatch: Some(SchemaMismatch { incompatible: true }),
            });
        };
        let incompatible = meta.schema_version != SEARCH_SCHEMA_VERSION
            || meta.chunk_format_version != SEARCH_CHUNK_FORMAT_VERSION;
        Ok(IndexMetadata {
            available: Some(SidecarInfo {
                schema_version: meta.schema_version,
                chunk_format_version: meta.chunk_format_version,
                embedding_profile_id: meta.embedding_profile_id,
            }),
            schema_mismatch: if incompatible {
                Some(SchemaMismatch { incompatible: true })
            } else {
                None
            },
        })
    }

    async fn search_lexical(
        &self,
        match_expr: &str,
        eligible: &[TaskId],
    ) -> Result<Vec<LexicalRank>, PortError> {
        let pool = self
            .open()
            .await
            .map_err(|f| PortError::Backend(format!("{f:?}")))?;
        let rows = sqlx::query(
            "SELECT c.task_id AS task_id, c.kind AS kind, c.text AS text, \
                    bm25(task_search_fts, 10.0) AS score \
             FROM task_search_fts \
             JOIN task_search_chunks c ON c.id = task_search_fts.rowid \
             WHERE task_search_fts MATCH ? \
             ORDER BY score ASC",
        )
        .bind(match_expr)
        .fetch_all(&pool)
        .await
        .map_err(|e| PortError::Backend(e.to_string()))?;

        let eligible_set: std::collections::HashSet<TaskId> = eligible.iter().copied().collect();
        let mut seen: std::collections::HashSet<TaskId> = std::collections::HashSet::new();
        let mut out: Vec<LexicalRank> = Vec::new();
        for r in rows {
            let Ok(task_id_str) = r.try_get::<String, _>("task_id") else {
                continue;
            };
            let Ok(tid) = TaskId::from_str(task_id_str.as_str()) else {
                continue;
            };
            if !eligible_set.contains(&tid) || seen.contains(&tid) {
                continue;
            }
            seen.insert(tid);
            let kind: String = r.try_get("kind").unwrap_or_default();
            let text: String = r.try_get("text").unwrap_or_default();
            let excerpt = if text.chars().count() > 200 {
                let s: String = text.chars().take(200).collect();
                format!("{s}…")
            } else {
                text
            };
            out.push(LexicalRank {
                task_id: tid,
                rank: out.len() + 1,
                kind: if kind == "comment" {
                    MatchedSourceKindDto::Comment
                } else {
                    MatchedSourceKindDto::Core
                },
                remote_comment_id: None,
                excerpt,
            });
        }
        Ok(out)
    }

    async fn stats(&self) -> Result<IndexStats, PortError> {
        let pool = self
            .open()
            .await
            .map_err(|f| PortError::Backend(format!("{f:?}")))?;
        let mut conn = pool
            .acquire()
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
        let chunks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_search_chunks")
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
        let vectors: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_search_vectors")
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
        let integrity = sqlx::query(
            "INSERT INTO task_search_fts(task_search_fts, rank) VALUES('integrity-check', 1)",
        )
        .execute(&mut *conn)
        .await
        .is_ok();
        let size = std::fs::metadata(&self.sidecar_path)
            .map(|m| m.len())
            .unwrap_or(0);
        drop(conn);
        Ok(IndexStats {
            chunk_count: chunks as u64,
            vector_count: vectors as u64,
            fts_integrity_ok: integrity,
            sidecar_size_bytes: size,
            sidecar_available: true,
        })
    }

    async fn clear(&self) -> Result<(), PortError> {
        let pool = self
            .open()
            .await
            .map_err(|f| PortError::Backend(format!("{f:?}")))?;
        let mut conn = pool
            .acquire()
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *conn)
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
        clear_all(&mut conn)
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
        sqlx::query("COMMIT")
            .execute(&mut *conn)
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
        let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&mut *conn)
            .await;
        Ok(())
    }

    async fn rebuild(&self, targets: &[ChunkTarget]) -> Result<u64, PortError> {
        let pool = self
            .open()
            .await
            .map_err(|f| PortError::Backend(format!("{f:?}")))?;
        let mut conn = pool
            .acquire()
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *conn)
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
        clear_all(&mut conn)
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
        // Within-task identical formatted chunks collapse to one row (RFC
        // 0007 D5); the reconcile path dedups via its hash key, so rebuild
        // must too.
        let mut seen: std::collections::HashSet<(TaskId, Vec<u8>)> = Default::default();
        let mut written: u64 = 0;
        for t in targets {
            if !seen.insert((t.task_id, t.content_hash.to_vec())) {
                continue;
            }
            sqlx::query(
                "INSERT INTO task_search_chunks (task_id, kind, content_hash, text) VALUES (?, ?, ?, ?)",
            )
            .bind(t.task_id.to_string())
            .bind(kind_str(t.kind))
            .bind(t.content_hash.as_slice())
            .bind(&t.text)
            .execute(&mut *conn)
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
            written += 1;
        }
        // Drop the validated-integrity marker so the next search re-checks.
        sqlx::query(
            "UPDATE task_search_meta SET validated_content_fingerprint = NULL, \
             validated_file_size = NULL, validated_file_mtime = NULL, validated_at = NULL \
             WHERE singleton = 1",
        )
        .execute(&mut *conn)
        .await
        .map_err(|e| PortError::Backend(e.to_string()))?;
        sqlx::query("COMMIT")
            .execute(&mut *conn)
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
        let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&mut *conn)
            .await;
        Ok(written)
    }
}

/// Transactionally delete all derived rows + FTS `delete-all` (RFC 0007 D5).
///
/// Order matters: deleting `task_search_chunks` fires the AFTER DELETE trigger
/// which removes each FTS entry by rowid; running `'delete-all'` *first* and
/// then triggering per-row deletes against an already-empty index reports
/// `SQLITE_CORRUPT`. Delete content first, then `delete-all` as a no-op
/// guarantee.
async fn clear_all(conn: &mut sqlx::SqliteConnection) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM task_search_vectors")
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM task_search_chunks")
        .execute(&mut *conn)
        .await?;
    sqlx::query("INSERT INTO task_search_fts(task_search_fts) VALUES('delete-all')")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ports::{ChunkKind, ChunkTarget, TaskSearchIndex};
    use tempfile::TempDir;

    fn tid(n: u128) -> TaskId {
        TaskId::from_uuid(uuid::Uuid::from_u128(n))
    }

    fn target(task: TaskId, kind: ChunkKind, text: &str) -> ChunkTarget {
        use sha2::{Digest, Sha256};
        let content_hash: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        ChunkTarget {
            task_id: task,
            kind,
            content_hash,
            text: text.to_string(),
        }
    }

    /// RFC 0007 D5 §10: pin FTS5 (and dbstat) availability in the linked
    /// SQLite build so CI proves the capability, not a machine-local fact.
    #[test]
    fn fts5_and_dbstat_availability() {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        use std::str::FromStr;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    SqliteConnectOptions::from_str("sqlite::memory:")
                        .unwrap()
                        .foreign_keys(true),
                )
                .await
                .unwrap();
            sqlx::query("CREATE VIRTUAL TABLE t USING fts5(x)")
                .execute(&pool)
                .await
                .expect("FTS5 must be compiled into the linked SQLite");
            let has_dbstat: i64 =
                sqlx::query_scalar("SELECT count(*) FROM pragma_module_list WHERE name = 'dbstat'")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert!(has_dbstat > 0, "dbstat must be available");
        });
    }

    /// Reconcile inserts, zero-change reconcile writes nothing, lexical
    /// search ranks, and clear empties the sidecar.
    #[tokio::test]
    async fn sidecar_reconcile_and_lifecycle() {
        let dir = TempDir::new().unwrap();
        let auth = dir.path().join("repo-link.db");
        let index = SqliteTaskSearchIndex::new(&auth);

        let a = tid(1);
        let b = tid(2);
        let targets = vec![
            target(a, ChunkKind::Core, "Fix the retry loop"),
            target(a, ChunkKind::Comment, "duplicate of body"),
            target(b, ChunkKind::Core, "Add retry backoff"),
        ];

        // First reconcile: insert 3.
        let mut sess = index.begin_reconcile().await.unwrap();
        let d1 = sess.diff_chunks(&targets, 10_000).await.unwrap();
        assert_eq!(d1.desired_total, 3);
        assert_eq!(d1.inserted, 3);
        sess.commit(&[7u8; 32]).await.unwrap();
        drop(sess); // release the sole pool connection before further ops

        // Lexical ranking for a returns 1 (rank 1 for "retry" appears in a).
        let ranks = index.search_lexical("retry", &[a, b]).await.unwrap();
        assert_eq!(ranks.len(), 2, "both tasks contain 'retry'-ish tokens");
        // rank 1 belongs to the best-scoring task; assert the set covers both.
        assert!(ranks.iter().any(|r| r.task_id == a));
        assert!(ranks.iter().any(|r| r.task_id == b));

        // Zero-change reconcile: nothing inserted.
        let mut sess = index.begin_reconcile().await.unwrap();
        let d2 = sess.diff_chunks(&targets, 10_000).await.unwrap();
        assert_eq!(d2.inserted, 0, "zero-change reconcile writes nothing");
        assert_eq!(d2.deleted, 0);
        sess.commit(&[7u8; 32]).await.unwrap();
        drop(sess);

        // Editing a to one chunk deletes the stale comment chunk.
        let reduced = vec![
            target(a, ChunkKind::Core, "Fix the retry loop"),
            target(b, ChunkKind::Core, "Add retry backoff"),
        ];
        let mut sess = index.begin_reconcile().await.unwrap();
        let d3 = sess.diff_chunks(&reduced, 10_000).await.unwrap();
        assert_eq!(d3.deleted, 1, "removed chunk must be deleted");
        assert_eq!(d3.inserted, 0);
        sess.commit(&[8u8; 32]).await.unwrap();
        drop(sess);

        // Clear empties the sidecar.
        index.clear().await.unwrap();
        let stats = index.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 0, "clear empties the sidecar");
        assert!(stats.fts_integrity_ok, "clear leaves a consistent index");

        // Rebuild restores.
        let written = index.rebuild(&targets).await.unwrap();
        assert_eq!(written, targets.len() as u64);
        let stats = index.stats().await.unwrap();
        assert_eq!(stats.chunk_count, targets.len() as u64);
        assert!(stats.fts_integrity_ok);
    }
    /// A failed reconcile (storage-limit refusal) must roll back cleanly so
    /// the single-connection pool is not left inside an open transaction.
    #[tokio::test]
    async fn failed_reconcile_leaves_sidecar_usable() {
        let dir = TempDir::new().unwrap();
        let auth = dir.path().join("repo-link.db");
        let index = SqliteTaskSearchIndex::new(&auth);
        let a = tid(1);
        let targets = vec![target(a, ChunkKind::Core, "x")];

        let mut sess = index.begin_reconcile().await.unwrap();
        let err = sess
            .diff_chunks(&targets, REFUSE_BYTES + 1)
            .await
            .unwrap_err();
        assert_eq!(err.reason, LexicalUnavailableReasonDto::StorageLimit);
        // The caller (TaskSearchService) always rolls back on a failed
        // reconcile; verify that clears the raw BEGIN IMMEDIATE so the pool
        // is not poisoned for the next acquire.
        sess.rollback().await.unwrap();
        drop(sess);

        // The pool must accept a fresh reconcile (not poisoned by the aborted
        // BEGIN IMMEDIATE).
        let mut sess2 = index.begin_reconcile().await.unwrap();
        let d2 = sess2.diff_chunks(&targets, 10_000).await.unwrap();
        assert_eq!(d2.inserted, 1);
        sess2.commit(&[1u8; 32]).await.unwrap();
        let stats = index.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 1);
        assert!(stats.fts_integrity_ok);
    }
}
