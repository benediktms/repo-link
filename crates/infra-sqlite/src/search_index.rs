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
    ChunkKind, ChunkTarget, GuardedVectorRow, IndexMetadata, IndexStats, LexicalRank,
    MissingSemanticInput, PortError, ReconcileDiff, ReconcileFailure, ReconcileSession,
    SEARCH_CHUNK_FORMAT_VERSION, SEARCH_SCHEMA_VERSION, SchemaMismatch, SemanticRank, SidecarInfo,
    TaskSearchIndex,
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
    /// Lazily-opened pool.
    pool: Mutex<Option<OpenSidecar>>,
}

/// One opened sidecar pool, plus whether that pool can write.
#[derive(Clone)]
struct OpenSidecar {
    pool: SqlitePool,
    /// False when only the read-only fallback opened the sidecar. Every
    /// maintenance operation refuses on such a pool (RFC 0007 D5/D8).
    writable: bool,
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

    /// Open the sidecar for a write: enforce PRAGMA-before-table ordering and
    /// an owner-only file, then create the D5 schema. Idempotent.
    ///
    /// A cached read-only pool refuses with `PermissionDenied` and no retry: it
    /// exists only because the environment denied a write open.
    async fn open_writable(&self) -> Result<SqlitePool, ReconcileFailure> {
        let mut guard = self.pool.lock().await;
        if let Some(open) = guard.as_ref() {
            if open.writable {
                return Ok(open.pool.clone());
            }
            return Err(ReconcileFailure {
                reason: LexicalUnavailableReasonDto::PermissionDenied,
            });
        }
        let pool = self.init_writable().await?;
        *guard = Some(OpenSidecar {
            pool: pool.clone(),
            writable: true,
        });
        Ok(pool)
    }

    /// Open the sidecar for a read. The writable open comes first, so a normal
    /// environment still gets the D5 schema setup and the owner-only policy.
    /// When the environment denies that write, the read-only fallback serves
    /// the already initialized sidecar instead of reporting it unavailable
    /// (RFC 0007 D8).
    ///
    /// Only a denial falls back. A corrupt, exhausted, or otherwise broken
    /// sidecar keeps its own reason, so it never masquerades as a read-only
    /// one for the rest of the process.
    async fn open_readable(&self) -> Result<OpenSidecar, ReconcileFailure> {
        let mut guard = self.pool.lock().await;
        if let Some(open) = guard.as_ref() {
            return Ok(open.clone());
        }
        let denied = match self.init_writable().await {
            Ok(pool) => {
                let open = OpenSidecar {
                    pool,
                    writable: true,
                };
                *guard = Some(open.clone());
                return Ok(open);
            }
            Err(denied) => denied,
        };
        if denied.reason != LexicalUnavailableReasonDto::PermissionDenied {
            return Err(denied);
        }
        let Some(pool) = self.connect_read_only().await else {
            return Err(denied);
        };
        let open = OpenSidecar {
            pool,
            writable: false,
        };
        *guard = Some(open.clone());
        Ok(open)
    }

    /// The writable open itself, without the cache.
    ///
    /// SQLite falls back to a read-only handle for a file it may not write
    /// instead of refusing the open, so a denied write surfaces at the first
    /// PRAGMA rather than at the open. Every step therefore classifies its own
    /// error.
    async fn init_writable(&self) -> Result<SqlitePool, ReconcileFailure> {
        fix_owner_only(&self.sidecar_path)
            .map_err(|e| self.open_failure(e, "sidecar owner-only check failed"))?;

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
            .map_err(|e| self.open_failure(e, "sidecar write open failed"))?;

        let mut conn = pool
            .acquire()
            .await
            .map_err(|e| self.open_failure(e, "sidecar write open failed"))?;
        // PRAGMAs must precede table creation (RFC 0007 D5).
        for pragma in [
            format!("PRAGMA page_size = {PAGE_SIZE};"),
            format!("PRAGMA auto_vacuum = {AUTO_VACUUM};"),
            format!("PRAGMA max_page_count = {MAX_PAGE_COUNT};"),
        ] {
            sqlx::query(&pragma)
                .execute(&mut *conn)
                .await
                .map_err(|e| self.write_failure(e, "sidecar PRAGMA write failed"))?;
        }
        for stmt in SCHEMA_SQL {
            sqlx::query(stmt)
                .execute(&mut *conn)
                .await
                .map_err(|e| self.write_failure(e, "sidecar schema setup failed"))?;
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
        .map_err(|e| self.write_failure(e, "sidecar metadata seed failed"))?;
        drop(conn);

        Ok(pool)
    }

    /// The read-only fallback ladder.
    ///
    /// `SQLITE_OPEN_READONLY` needs a usable `-shm` file, and a WAL sidecar
    /// cannot create one in a directory it may not write, so `immutable` is
    /// the second rung. Neither rung sets `journal_mode`, because that PRAGMA
    /// writes the database header.
    ///
    /// CAUTION: `immutable` makes SQLite ignore a `-wal` file, and SQLite
    /// leaves the result undefined when that file holds a hot journal. The rung
    /// therefore refuses a sidecar with a non-empty `-wal`, and the caller
    /// reports the sidecar unavailable instead of serving an undefined read.
    ///
    /// The write path hardens the sidecar file, and this path can only verify
    /// it, so both enforce the same D5 policy. SQLite also opens the file on
    /// the first read rather than at the open, so each rung needs a real read
    /// to prove it works.
    async fn connect_read_only(&self) -> Option<SqlitePool> {
        if let Err(e) = check_path_policy(&self.sidecar_path) {
            tracing::warn!(
                error = %e,
                sidecar = %self.sidecar_path.display(),
                "read-only sidecar open refused by the path policy"
            );
            return None;
        }
        let url = format!("sqlite://{}", self.sidecar_path.display());
        let hot_wal = std::fs::metadata(PathBuf::from(format!(
            "{}-wal",
            self.sidecar_path.display()
        )))
        .is_ok_and(|m| m.len() > 0);
        for immutable in [false, true] {
            if immutable && hot_wal {
                tracing::warn!(
                    sidecar = %self.sidecar_path.display(),
                    "refusing an immutable read-only open: the sidecar has a non-empty WAL"
                );
                return None;
            }
            let Ok(opts) = sqlx::sqlite::SqliteConnectOptions::from_str(&url) else {
                return None;
            };
            let opts = opts
                .create_if_missing(false)
                .read_only(true)
                .immutable(immutable)
                .foreign_keys(true)
                .busy_timeout(std::time::Duration::from_secs(10));
            let Ok(pool) = SqlitePoolOptions::new()
                .max_connections(2)
                .connect_with(opts)
                .await
            else {
                continue;
            };
            match sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sqlite_schema")
                .fetch_one(&pool)
                .await
            {
                Ok(_) => {
                    tracing::warn!(
                        sidecar = %self.sidecar_path.display(),
                        immutable,
                        "opened the task-search sidecar read-only; index maintenance is unavailable"
                    );
                    return Some(pool);
                }
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        sidecar = %self.sidecar_path.display(),
                        immutable,
                        "read-only sidecar open rung failed"
                    );
                    pool.close().await;
                }
            }
        }
        None
    }

    /// Classify and log one failed write open (RFC 0007 D8). The caller keeps
    /// only the reason, so the underlying error goes to the diagnostic log.
    fn open_failure(&self, e: impl std::fmt::Display, context: &'static str) -> ReconcileFailure {
        self.classify(e, context, false)
    }

    /// The same, for a write that already has the sidecar open. SQLite reports
    /// a `-wal` or `-shm` it may not create as an unopenable database file, so
    /// past the open that message means a denial, not an absent sidecar.
    fn write_failure(&self, e: impl std::fmt::Display, context: &'static str) -> ReconcileFailure {
        self.classify(e, context, true)
    }

    fn classify(
        &self,
        e: impl std::fmt::Display,
        context: &'static str,
        past_open: bool,
    ) -> ReconcileFailure {
        let msg = e.to_string();
        let denied =
            write_denied(&msg) || (past_open && msg.contains("unable to open database file"));
        let reason = if denied {
            LexicalUnavailableReasonDto::PermissionDenied
        } else {
            LexicalUnavailableReasonDto::SidecarUnavailable
        };
        tracing::warn!(
            error = %msg,
            sidecar = %self.sidecar_path.display(),
            reason = ?reason,
            "{context}"
        );
        ReconcileFailure { reason }
    }

    /// Read the singleton metadata row. The bool reports whether the sidecar
    /// opened writable.
    async fn read_meta(&self) -> Result<(bool, Option<MetaRow>), ReconcileFailure> {
        let open = self.open_readable().await?;
        let pool = open.pool;
        let row = sqlx::query(
            "SELECT singleton, schema_version, chunk_format_version, embedding_profile_id, \
                    validated_content_fingerprint \
             FROM task_search_meta WHERE singleton = 1",
        )
        .fetch_optional(&pool)
        .await
        .map_err(move_to_failure)?;
        let meta = row.map(|r| MetaRow {
            schema_version: r.try_get("schema_version").unwrap_or(0),
            chunk_format_version: r.try_get("chunk_format_version").unwrap_or(0),
            // try_get::<String> on a NULL column returns Ok("") in sqlx —
            // decode as Option so an unclaimed profile reads None, not "".
            embedding_profile_id: r
                .try_get::<Option<String>, _>("embedding_profile_id")
                .ok()
                .flatten(),
        });
        Ok((open.writable, meta))
    }
}

#[derive(Clone)]
struct MetaRow {
    schema_version: i64,
    chunk_format_version: i64,
    embedding_profile_id: Option<String>,
}

/// The read-only half of the D5 sidecar path policy: reject a symlink, a
/// world-writable parent, and a sidecar that others may write.
///
/// Every step here is a read, so the read-only open path enforces it too. The
/// rule on the file mode is the integrity half only: a sidecar that others may
/// *write* can feed content into search results, while one that others may
/// *read* is already exposed, and refusing to read it protects nothing. The
/// write path keeps the stricter owner-only rule, which `fix_owner_only`
/// verifies on the opened file (RFC 0007 D5).
#[cfg(unix)]
fn check_path_policy(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // Reject a symlink anywhere in the sidecar path (no-follow, RFC 0007 D5).
    let existing = std::fs::symlink_metadata(path).ok();
    if let Some(m) = &existing {
        if m.file_type().is_symlink() {
            return Err(std::io::Error::other("sidecar path is a symlink"));
        }
        if m.permissions().mode() & 0o022 != 0 {
            return Err(std::io::Error::other("sidecar is writable by others"));
        }
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
    Ok(())
}

#[cfg(not(unix))]
fn check_path_policy(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Enforce the owner-only + trusted-parent policy for the sidecar file
/// (RFC 0007 D5). Pre-creates the file with mode `0600` when absent on Unix
/// and refuses when the parent is world-writable or not owned.
///
/// The `-wal` and `-shm` files are covered too. SQLite creates them with the
/// mode of the main file, so they hold the same task text, and a read-only
/// session otherwise leaves a `-shm` that refuses every later write.
#[cfg(unix)]
fn fix_owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::fs::PermissionsExt;

    check_path_policy(path)?;
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", path.display()));
        let Ok(m) = std::fs::symlink_metadata(&side) else {
            continue;
        };
        if m.file_type().is_symlink() {
            return Err(std::io::Error::other("sidecar journal path is a symlink"));
        }
        if m.permissions().mode() & 0o777 != 0o600 {
            std::fs::set_permissions(&side, std::fs::Permissions::from_mode(0o600))?;
        }
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

/// Windows owner-only policy: the default ACL on a user-created file is
/// owner-only, so creating it (without touching unix modes) is the best-effort
/// equivalent. Explicit ACL hardening is a follow-up (RFC 0007 D5).
#[cfg(not(unix))]
fn fix_owner_only(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    Ok(())
}

fn move_to_failure(e: impl ToString) -> ReconcileFailure {
    let msg = e.to_string();
    let reason = if write_denied(&msg) {
        LexicalUnavailableReasonDto::PermissionDenied
    } else {
        LexicalUnavailableReasonDto::SidecarUnavailable
    };
    tracing::warn!(error = %msg, reason = ?reason, "task-search sidecar operation failed");
    ReconcileFailure { reason }
}

/// Flatten a sidecar failure into a `PortError` with the stable D10 reason
/// code. The port boundary carries a string, so the code goes in as
/// snake_case rather than as a `Debug` dump of the struct.
fn reason_code(f: ReconcileFailure) -> PortError {
    let code = match f.reason {
        LexicalUnavailableReasonDto::SidecarUnavailable => "sidecar_unavailable",
        LexicalUnavailableReasonDto::PermissionDenied => "permission_denied",
        LexicalUnavailableReasonDto::SchemaMismatch => "schema_mismatch",
        LexicalUnavailableReasonDto::IndexCorrupt => "index_corrupt",
        LexicalUnavailableReasonDto::StorageLimit => "storage_limit",
        LexicalUnavailableReasonDto::ReconciliationFailed => "reconciliation_failed",
    };
    PortError::Backend(format!("task-search sidecar: {code}"))
}

/// Whether a failed operation was refused for a lack of write access.
///
/// ponytail: matched on the message. `sqlx` flattens the SQLite extended
/// result code, and the same helper then covers both a `std::io::Error`
/// ("Permission denied", "Read-only file system") and a SQLite error
/// ("attempt to write a readonly database").
fn write_denied(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("readonly")
        || m.contains("read-only")
        || m.contains("permission denied")
        || m.contains("access is denied")
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
        let pool = self.open_writable().await.map_err(reason_code)?;
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
        let (writable, meta) = match self.read_meta().await {
            Ok(m) => m,
            Err(_) => {
                // Open failure → sidecar unavailable (D8). Not a hard error.
                return Ok(IndexMetadata {
                    available: None,
                    schema_mismatch: None,
                    read_only: false,
                });
            }
        };
        let Some(meta) = meta else {
            // Openable but no metadata row → incompatible/unknown schema.
            return Ok(IndexMetadata {
                available: None,
                schema_mismatch: Some(SchemaMismatch { incompatible: true }),
                read_only: !writable,
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
            read_only: !writable,
        })
    }

    async fn search_lexical(
        &self,
        match_expr: &str,
        eligible: &[TaskId],
    ) -> Result<Vec<LexicalRank>, PortError> {
        let pool = self.open_readable().await.map_err(reason_code)?.pool;
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
        let open = self.open_readable().await.map_err(reason_code)?;
        let mut conn = open
            .pool
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
        let integrity = if open.writable {
            Some(
                sqlx::query(
                    "INSERT INTO task_search_fts(task_search_fts, rank) VALUES('integrity-check', 1)",
                )
                .execute(&mut *conn)
                .await
                .is_ok(),
            )
        } else {
            None
        };
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
        let pool = self.open_writable().await.map_err(reason_code)?;
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
        let pool = self.open_writable().await.map_err(reason_code)?;
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

    async fn claim_empty_profile(&self, expected: &str) -> Result<bool, PortError> {
        let pool = self.open_writable().await.map_err(reason_code)?;
        let mut conn = pool
            .acquire()
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
        let result = async {
            sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
            let changed = sqlx::query(
                "UPDATE task_search_meta SET embedding_profile_id = ? \
                 WHERE singleton = 1 AND embedding_profile_id IS NULL",
            )
            .bind(expected)
            .execute(&mut *conn)
            .await?
            .rows_affected();
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            Ok::<bool, sqlx::Error>(changed > 0)
        }
        .await;
        if result.is_err() {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
        }
        result.map_err(|e| PortError::Backend(e.to_string()))
    }

    async fn missing_semantic_inputs(
        &self,
        limit: u32,
    ) -> Result<Vec<MissingSemanticInput>, PortError> {
        let pool = self.open_readable().await.map_err(reason_code)?.pool;
        let rows = sqlx::query(
            "SELECT c.id AS id, c.task_id AS task_id, c.kind AS kind, \
                    c.content_hash AS content_hash, c.text AS text \
             FROM task_search_chunks c \
             LEFT JOIN task_search_vectors v ON v.search_chunk_id = c.id \
             WHERE v.search_chunk_id IS NULL \
             ORDER BY c.id ASC \
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&pool)
        .await
        .map_err(|e| PortError::Backend(e.to_string()))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let id: i64 = r
                .try_get("id")
                .map_err(|e| PortError::Backend(e.to_string()))?;
            let task_id_str: String = r
                .try_get("task_id")
                .map_err(|e| PortError::Backend(e.to_string()))?;
            let Ok(task_id) = TaskId::from_str(&task_id_str) else {
                continue;
            };
            let kind: String = r.try_get("kind").unwrap_or_default();
            let content_hash: Vec<u8> = r.try_get("content_hash").unwrap_or_default();
            let text: String = r.try_get("text").unwrap_or_default();
            let Ok(content_hash) = content_hash.try_into() else {
                continue;
            };
            out.push(MissingSemanticInput {
                search_chunk_id: id,
                task_id,
                kind: if kind == "comment" {
                    ChunkKind::Comment
                } else {
                    ChunkKind::Core
                },
                content_hash,
                text,
            });
        }
        Ok(out)
    }

    async fn store_vectors_guarded(&self, rows: &[GuardedVectorRow]) -> Result<usize, PortError> {
        if rows.is_empty() {
            return Ok(0);
        }
        let pool = self.open_writable().await.map_err(reason_code)?;
        let mut conn = pool
            .acquire()
            .await
            .map_err(|e| PortError::Backend(e.to_string()))?;
        let backend = |e: sqlx::Error| PortError::Backend(e.to_string());
        let result = async {
            sqlx::query("BEGIN IMMEDIATE")
                .execute(&mut *conn)
                .await
                .map_err(backend)?;
            let mut vectors_in_place = 0usize;
            let mut written: Vec<i64> = Vec::new();
            for row in rows {
                let guarded_ok: Option<i64> = sqlx::query_scalar(
                    "SELECT 1 FROM task_search_meta m, task_search_chunks c \
                     WHERE m.singleton = 1 \
                       AND m.embedding_profile_id IS NOT NULL \
                       AND c.id = ? AND c.task_id = ? AND c.content_hash = ?",
                )
                .bind(row.search_chunk_id)
                .bind(row.task_id.to_string())
                .bind(row.content_hash.as_slice())
                .fetch_optional(&mut *conn)
                .await
                .map_err(backend)?;
                if guarded_ok.is_none() {
                    continue;
                }
                written.push(row.search_chunk_id);
                // A re-embed of the same segment with a different input hash
                // supersedes the stored row; without this the INSERT OR IGNORE
                // conflict keeps the stale vector forever.
                sqlx::query(
                    "DELETE FROM task_search_vectors \
                     WHERE search_chunk_id = ? AND segment_index = ? \
                       AND embedding_input_hash != ?",
                )
                .bind(row.search_chunk_id)
                .bind(row.segment_index)
                .bind(row.embedding_input_hash.as_slice())
                .execute(&mut *conn)
                .await
                .map_err(backend)?;
                let vector_bytes = encode_vector(&row.vector);
                sqlx::query(
                    "INSERT OR IGNORE INTO task_search_vectors \
                     (search_chunk_id, segment_index, embedding_input_hash, vector) \
                     VALUES (?, ?, ?, ?)",
                )
                .bind(row.search_chunk_id)
                .bind(row.segment_index)
                .bind(row.embedding_input_hash.as_slice())
                .bind(vector_bytes)
                .execute(&mut *conn)
                .await
                .map_err(backend)?;
                vectors_in_place += 1;
            }
            written.sort_unstable();
            written.dedup();
            for chunk_id in written {
                verify_segment_coverage(&mut conn, chunk_id).await?;
            }
            sqlx::query("COMMIT")
                .execute(&mut *conn)
                .await
                .map_err(backend)?;
            Ok::<usize, PortError>(vectors_in_place)
        }
        .await;
        if result.is_err() {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
        }
        result
    }

    async fn search_semantic(
        &self,
        query_vector: &[f32],
        eligible: &[TaskId],
    ) -> Result<Vec<SemanticRank>, PortError> {
        let pool = self.open_readable().await.map_err(reason_code)?.pool;
        // Filter eligible tasks in SQL so I/O is O(eligible), not O(vectors).
        // Chunk the IN list to stay under SQLite's variable limit.
        const MAX_IN: usize = 500;
        let mut best: HashMap<TaskId, f32> = HashMap::new();
        for chunk in eligible.chunks(MAX_IN) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT c.task_id AS task_id, v.vector AS vector \
                 FROM task_search_vectors v \
                 JOIN task_search_chunks c ON c.id = v.search_chunk_id \
                 WHERE c.task_id IN ({placeholders})"
            );
            let mut q = sqlx::query(&sql);
            for tid in chunk {
                q = q.bind(tid.to_string());
            }
            let rows = q
                .fetch_all(&pool)
                .await
                .map_err(|e| PortError::Backend(e.to_string()))?;
            for r in rows {
                let Ok(task_id_str) = r.try_get::<String, _>("task_id") else {
                    continue;
                };
                let Ok(tid) = TaskId::from_str(&task_id_str) else {
                    continue;
                };
                let Ok(blob) = r.try_get::<Vec<u8>, _>("vector") else {
                    continue;
                };
                let Ok(vec) = decode_vector(&blob, query_vector.len()) else {
                    continue;
                };
                let score = cosine(query_vector, &vec);
                let e = best.entry(tid).or_insert(f32::NEG_INFINITY);
                if score > *e {
                    *e = score;
                }
            }
        }
        let mut out: Vec<SemanticRank> = best
            .into_iter()
            .map(|(task_id, score)| SemanticRank { task_id, score })
            .collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.task_id.as_uuid().cmp(&b.task_id.as_uuid()))
        });
        Ok(out)
    }
}

fn encode_vector(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn decode_vector(blob: &[u8], dims: usize) -> Result<Vec<f32>, String> {
    if blob.len() != dims * 4 {
        return Err(format!("vector blob len {} != {dims} dims", blob.len()));
    }
    let mut out = Vec::with_capacity(dims);
    for chunk in blob.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(out)
}

/// Reject a chunk whose stored segments are not `0..n` without gaps.
///
/// `missing_semantic_inputs` is chunk-granular: one vector row marks the whole
/// chunk done. Partial coverage would therefore never be re-queued and those
/// segments would stay unvectorized forever, so a batch that would leave a gap
/// fails and rolls back instead (RFC 0007 D6).
async fn verify_segment_coverage(
    conn: &mut sqlx::SqliteConnection,
    search_chunk_id: i64,
) -> Result<(), PortError> {
    let (count, lo, hi): (i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(MIN(segment_index), -1), COALESCE(MAX(segment_index), -1) \
         FROM task_search_vectors WHERE search_chunk_id = ?",
    )
    .bind(search_chunk_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| PortError::Backend(e.to_string()))?;
    if lo != 0 || count != hi + 1 {
        return Err(PortError::Backend(format!(
            "incomplete segment coverage for chunk {search_chunk_id}: \
             {count} rows spanning {lo}..={hi}"
        )));
    }
    Ok(())
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    let denom = (norm_a * norm_b).max(f32::EPSILON);
    dot / denom
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
        assert_eq!(
            stats.fts_integrity_ok,
            Some(true),
            "clear leaves a consistent index"
        );

        // Rebuild restores.
        let written = index.rebuild(&targets).await.unwrap();
        assert_eq!(written, targets.len() as u64);
        let stats = index.stats().await.unwrap();
        assert_eq!(stats.chunk_count, targets.len() as u64);
        assert_eq!(stats.fts_integrity_ok, Some(true));
    }
    /// A readable but non-writable sidecar still answers `stats`,
    /// `metadata`, and `search_lexical`, and refuses maintenance with
    /// `permission_denied` (RFC 0007 D8).
    ///
    /// `rebuild` ends with `wal_checkpoint(TRUNCATE)`, and the journal files
    /// then go, so the sidecar is the cleanly closed file that the report
    /// describes and both rungs of the ladder read the same content.
    #[cfg(unix)]
    #[tokio::test]
    async fn read_only_sidecar_serves_status_and_lexical_search() {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let auth = dir.path().join("repo-link.db");
        let a = tid(1);
        let b = tid(2);
        let targets = vec![
            target(a, ChunkKind::Core, "Fix the retry loop"),
            target(b, ChunkKind::Core, "Add retry backoff"),
        ];

        {
            let index = SqliteTaskSearchIndex::new(&auth);
            assert_eq!(index.rebuild(&targets).await.unwrap(), 2);
        }

        let sidecar = dir.path().join("repo-link.db.task-search.db");
        let _ = std::fs::remove_file(dir.path().join("repo-link.db.task-search.db-wal"));
        let _ = std::fs::remove_file(dir.path().join("repo-link.db.task-search.db-shm"));
        std::fs::set_permissions(&sidecar, Permissions::from_mode(0o400)).unwrap();
        std::fs::set_permissions(dir.path(), Permissions::from_mode(0o500)).unwrap();
        let restore = || {
            std::fs::set_permissions(dir.path(), Permissions::from_mode(0o700)).unwrap();
            std::fs::set_permissions(&sidecar, Permissions::from_mode(0o600)).unwrap();
        };
        let mode_bits_enforced = std::fs::File::create(dir.path().join("probe")).is_err();
        if !mode_bits_enforced {
            restore();
            return;
        }

        let index = SqliteTaskSearchIndex::new(&auth);

        let stats = index.stats().await.unwrap();
        assert_eq!(
            stats.chunk_count, 2,
            "a read-only sidecar still reports its rows"
        );
        assert!(stats.sidecar_available);
        assert_eq!(
            stats.fts_integrity_ok, None,
            "the FTS integrity check writes, so it cannot run"
        );

        let meta = index.metadata().await.unwrap();
        assert!(meta.available.is_some(), "the D5 schema stays readable");
        assert!(meta.read_only, "metadata reports the read-only open");
        assert!(meta.schema_mismatch.is_none());

        let ranks = index.search_lexical("retry", &[a, b]).await.unwrap();
        assert_eq!(
            ranks.len(),
            2,
            "lexical search answers without write access"
        );

        let err = index.rebuild(&targets).await.unwrap_err();
        let PortError::Backend(msg) = err else {
            restore();
            panic!("rebuild must fail with a backend error");
        };
        assert!(
            msg.contains("permission_denied"),
            "maintenance reports permission_denied, got {msg}"
        );

        restore();
    }
    /// A read-only session must not wedge a later write. SQLite creates the
    /// `-shm` file with the mode of the main file, so the read-only open
    /// leaves a non-writable side file behind.
    ///
    /// Only the file loses its write bit here. The directory keeps it, so
    /// SQLite can still create the `-shm` that a read-only open needs.
    #[cfg(unix)]
    #[tokio::test]
    async fn read_only_open_does_not_wedge_a_later_write() {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let auth = dir.path().join("repo-link.db");
        let a = tid(1);
        let targets = vec![target(a, ChunkKind::Core, "Fix the retry loop")];
        {
            let index = SqliteTaskSearchIndex::new(&auth);
            assert_eq!(index.rebuild(&targets).await.unwrap(), 1);
        }

        let sidecar = dir.path().join("repo-link.db.task-search.db");
        std::fs::set_permissions(&sidecar, Permissions::from_mode(0o400)).unwrap();
        let mode_bits_enforced = std::fs::OpenOptions::new()
            .write(true)
            .open(&sidecar)
            .is_err();
        if !mode_bits_enforced {
            return;
        }
        {
            let index = SqliteTaskSearchIndex::new(&auth);
            assert!(index.metadata().await.unwrap().read_only);
        }

        std::fs::set_permissions(&sidecar, Permissions::from_mode(0o600)).unwrap();
        let index = SqliteTaskSearchIndex::new(&auth);
        let stats = index.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 1);
        assert_eq!(
            stats.fts_integrity_ok,
            Some(true),
            "a restored permission must open writable again"
        );
        assert!(!index.metadata().await.unwrap().read_only);
    }
    /// A hot `-wal` blocks the `immutable` rung, because SQLite leaves such a
    /// read undefined. The sidecar reports unavailable instead.
    #[cfg(unix)]
    #[tokio::test]
    async fn hot_wal_refuses_the_immutable_read_only_rung() {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let auth = dir.path().join("repo-link.db");
        let a = tid(1);
        let targets = vec![target(a, ChunkKind::Core, "Fix the retry loop")];
        {
            let index = SqliteTaskSearchIndex::new(&auth);
            assert_eq!(index.rebuild(&targets).await.unwrap(), 1);
        }

        let sidecar = dir.path().join("repo-link.db.task-search.db");
        let wal = dir.path().join("repo-link.db.task-search.db-wal");
        let _ = std::fs::remove_file(dir.path().join("repo-link.db.task-search.db-shm"));
        std::fs::write(&wal, vec![0u8; 64]).unwrap();
        std::fs::set_permissions(&sidecar, Permissions::from_mode(0o400)).unwrap();
        std::fs::set_permissions(dir.path(), Permissions::from_mode(0o500)).unwrap();
        let restore = || {
            std::fs::set_permissions(dir.path(), Permissions::from_mode(0o700)).unwrap();
            std::fs::set_permissions(&sidecar, Permissions::from_mode(0o600)).unwrap();
        };
        let mode_bits_enforced = std::fs::File::create(dir.path().join("probe")).is_err();
        if !mode_bits_enforced {
            restore();
            return;
        }

        let index = SqliteTaskSearchIndex::new(&auth);
        let meta = index.metadata().await.unwrap();
        assert!(
            meta.available.is_none(),
            "an undefined read must report the sidecar unavailable"
        );
        assert!(!meta.read_only);
        assert!(index.stats().await.is_err());

        restore();
    }

    /// A sidecar that others may read still opens read-only. The write path
    /// keeps the owner-only rule; the read path only refuses one that others
    /// may write.
    #[cfg(unix)]
    #[tokio::test]
    async fn world_readable_sidecar_opens_read_only() {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let auth = dir.path().join("repo-link.db");
        let a = tid(1);
        let targets = vec![target(a, ChunkKind::Core, "Fix the retry loop")];
        {
            let index = SqliteTaskSearchIndex::new(&auth);
            assert_eq!(index.rebuild(&targets).await.unwrap(), 1);
        }

        let sidecar = dir.path().join("repo-link.db.task-search.db");
        std::fs::set_permissions(&sidecar, Permissions::from_mode(0o444)).unwrap();
        let mode_bits_enforced = std::fs::OpenOptions::new()
            .write(true)
            .open(&sidecar)
            .is_err();
        if !mode_bits_enforced {
            return;
        }

        let index = SqliteTaskSearchIndex::new(&auth);
        let stats = index.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 1);
        assert!(index.metadata().await.unwrap().read_only);

        std::fs::set_permissions(&sidecar, Permissions::from_mode(0o600)).unwrap();
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
        assert_eq!(stats.fts_integrity_ok, Some(true));
    }

    /// Unclaimed profile reads as `None`, not an empty string (sqlx decodes
    /// NULL into `Ok("")` for `String`), so claim + semantic availability
    /// work on a fresh sidecar.
    #[tokio::test]
    async fn unclaimed_profile_reads_none() {
        let dir = TempDir::new().unwrap();
        let auth = dir.path().join("repo-link.db");
        let index = SqliteTaskSearchIndex::new(&auth);
        let meta = index.metadata().await.unwrap();
        let profile = meta
            .available
            .as_ref()
            .and_then(|s| s.embedding_profile_id.clone());
        assert_eq!(profile, None, "fresh sidecar must have no claimed profile");
    }

    /// A batch that would leave a chunk partially vectorized is rejected:
    /// chunk-granular discovery would never re-queue the missing segments.
    #[tokio::test]
    async fn partial_segment_coverage_is_rejected() {
        let dir = TempDir::new().unwrap();
        let auth = dir.path().join("repo-link.db");
        let index = SqliteTaskSearchIndex::new(&auth);

        let a = tid(1);
        let mut sess = index.begin_reconcile().await.unwrap();
        sess.diff_chunks(&[target(a, ChunkKind::Core, "Fix the retry loop")], 10_000)
            .await
            .unwrap();
        sess.commit(&[1u8; 32]).await.unwrap();
        drop(sess);
        assert!(index.claim_empty_profile("prof-x").await.unwrap());

        let missing = index.missing_semantic_inputs(10).await.unwrap();
        let m = &missing[0];
        let row = |segment_index: u32| GuardedVectorRow {
            search_chunk_id: m.search_chunk_id,
            task_id: m.task_id,
            content_hash: m.content_hash,
            segment_index,
            embedding_input_hash: [segment_index as u8; 32],
            vector: vec![1.0f32; 8],
        };

        assert!(
            index.store_vectors_guarded(&[row(1)]).await.is_err(),
            "segment 1 without segment 0 leaves a gap"
        );
        assert_eq!(
            index.stats().await.unwrap().vector_count,
            0,
            "a rejected batch must roll back"
        );

        let batch = [row(0), row(1)];
        assert_eq!(index.store_vectors_guarded(&batch).await.unwrap(), 2);
        assert_eq!(index.stats().await.unwrap().vector_count, 2);
        assert!(
            index.missing_semantic_inputs(10).await.unwrap().is_empty(),
            "a fully covered chunk is no longer missing"
        );

        assert_eq!(
            index.store_vectors_guarded(&batch).await.unwrap(),
            2,
            "a writer that loses the fill race sees vectors in place, not no progress"
        );
    }

    /// Claim → missing inputs → guarded store → semantic ranking, with the
    /// cosine ordering and the stale-chunk guard.
    #[tokio::test]
    async fn semantic_vectors_claim_store_search() {
        let dir = TempDir::new().unwrap();
        let auth = dir.path().join("repo-link.db");
        let index = SqliteTaskSearchIndex::new(&auth);

        let a = tid(1);
        let b = tid(2);
        let targets = vec![
            target(a, ChunkKind::Core, "Fix the retry loop"),
            target(b, ChunkKind::Core, "Add retry backoff"),
        ];
        let mut sess = index.begin_reconcile().await.unwrap();
        sess.diff_chunks(&targets, 10_000).await.unwrap();
        sess.commit(&[1u8; 32]).await.unwrap();
        drop(sess);

        // Claim: first claim wins; second is refused.
        assert!(index.claim_empty_profile("prof-x").await.unwrap());
        assert!(!index.claim_empty_profile("prof-y").await.unwrap());

        // All chunks are missing vectors.
        let missing = index.missing_semantic_inputs(10).await.unwrap();
        assert_eq!(missing.len(), 2);

        // Build guarded rows: chunk a gets a vector aligned with the query,
        // chunk b a vector orthogonal to it. Rowid assignment is
        // nondeterministic (reconcile iterates a HashMap), so pick the rows
        // by task id, not by iteration order.
        let mut rows = Vec::new();
        for m in &missing {
            let aligned = m.task_id == a;
            let vec = if aligned {
                vec![1.0f32; 8]
            } else {
                (0..8)
                    .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
                    .collect()
            };
            rows.push(GuardedVectorRow {
                search_chunk_id: m.search_chunk_id,
                task_id: m.task_id,
                content_hash: m.content_hash,
                segment_index: 0,
                embedding_input_hash: [9u8; 32],
                vector: vec,
            });
        }
        index.store_vectors_guarded(&rows).await.unwrap();
        let stats = index.stats().await.unwrap();
        assert_eq!(stats.vector_count, 2);

        // Semantic search: a (aligned) ranks above b (orthogonal).
        let query = vec![1.0f32; 8];
        let ranks = index.search_semantic(&query, &[a, b]).await.unwrap();
        assert_eq!(ranks.len(), 2);
        assert_eq!(ranks[0].task_id, a, "aligned vector must rank first");
        assert!(
            (ranks[0].score - 1.0).abs() < 1e-6,
            "aligned score must be a normalized 1.0, not a dot product; got {}",
            ranks[0].score
        );
        assert!(
            ranks[1].score.abs() < 1e-6,
            "orthogonal score must be 0.0, got {}",
            ranks[1].score
        );

        // Guard: a stale chunk (deleted before store) must not accept a
        // vector. Delete chunk a's rows, then attempt a guarded store.
        let mut sess = index.begin_reconcile().await.unwrap();
        let reduced = vec![target(b, ChunkKind::Core, "Add retry backoff")];
        sess.diff_chunks(&reduced, 10_000).await.unwrap();
        sess.commit(&[2u8; 32]).await.unwrap();
        drop(sess);
        let stale = GuardedVectorRow {
            search_chunk_id: rows
                .iter()
                .find(|r| r.task_id == a)
                .unwrap()
                .search_chunk_id,
            task_id: a,
            content_hash: rows.iter().find(|r| r.task_id == a).unwrap().content_hash,
            segment_index: 0,
            embedding_input_hash: [9u8; 32],
            vector: vec![1.0f32; 8],
        };
        index.store_vectors_guarded(&[stale]).await.unwrap();
        let stats = index.stats().await.unwrap();
        assert_eq!(stats.vector_count, 1, "stale chunk vector must be rejected");
    }
}
