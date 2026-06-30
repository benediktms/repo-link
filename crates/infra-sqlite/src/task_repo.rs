use async_trait::async_trait;
use chrono::{DateTime, Utc};
use domain_core::{RepoId, RepoOriginId, TaskId, Timestamp, WorkspaceId};
use domain_sync::OutboxEntry;
use domain_task::{
    Lifecycle, Priority, RelationKind, RemoteRef, SnapshotSource, SyncState, Task, TaskComment,
    TaskRelation, TaskSnapshot,
};
use ports::{PortError, PortResult, RemoteComment, SyncedSource, TaskFilter, TaskRepository};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};

use crate::Db;
use crate::mapping::{
    enum_from_str, enum_to_str, json_from_string, json_to_string, map_sqlx_err, parse_uuid,
};

pub struct SqliteTaskRepository {
    db: Db,
}

impl SqliteTaskRepository {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

// `filing_repo_id` is listed here to satisfy the schema-consistency contract
// (the const must name every live column, see #110) but is NOT yet read by
// `row_to_task` — the domain `Task` gains the field in #116. Selecting an
// unmapped column is harmless: `row_to_task` extracts by name and ignores it.
// The legacy `status` column is still listed (it remains a live NOT NULL column,
// per the #110 every-live-column contract) but `row_to_task` no longer reads it —
// the lifecycle axis is read from `lifecycle` (RFC 0004 D1). `synced_at` is the
// RFC 0004 D3 write-through cache column.
pub(crate) const TASK_COLS: &str = "id, workspace_id, repo_instance_id, title, body, status, sync_state, priority, assignees_json, remote_provider, remote_id, created_at, updated_at, hash, project_item_id, remote_node_id, project_status_option_id, filing_repo_id, lifecycle, synced_at";

#[async_trait]
impl TaskRepository for SqliteTaskRepository {
    async fn save(&self, t: &Task, source: SnapshotSource) -> PortResult<()> {
        // BEGIN IMMEDIATE: take the writer lock up front so we don't risk a
        // mid-flight SQLITE_BUSY during the parent + relations + remote
        // mapping + snapshot multi-step write.
        let mut tx = self
            .db
            .writes
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_sqlx_err)?;
        write_task_in_tx(&mut tx, t, source).await?;
        tx.commit().await.map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn save_many(&self, tasks: &[(&Task, SnapshotSource)]) -> PortResult<()> {
        // One transaction spanning every task's write set, so the reciprocal
        // sides of a relation edge either both persist or neither does — a
        // mid-batch failure can't leave the graph asymmetric. BEGIN IMMEDIATE
        // grabs the writer lock once for the whole batch.
        let mut tx = self
            .db
            .writes
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_sqlx_err)?;
        for (t, source) in tasks {
            write_task_in_tx(&mut tx, t, *source).await?;
        }
        tx.commit().await.map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn save_with_outbox(
        &self,
        t: &Task,
        source: SnapshotSource,
        entries: &[OutboxEntry],
    ) -> PortResult<()> {
        // Transactional outbox (#54, thread r3324166852): the task write (row +
        // snapshot + relations + remote mapping) AND the outbox entries land in
        // ONE transaction, so a crash can't leave a saved mirror task with no
        // durable outbox entry. BEGIN IMMEDIATE takes the writer lock once for
        // the whole unit. The outbox repo wraps the SAME pool, so its
        // `insert_outbox_in_tx` writer slots straight into this transaction —
        // no duplicated INSERT SQL.
        let mut tx = self
            .db
            .writes
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_sqlx_err)?;
        write_task_in_tx(&mut tx, t, source).await?;
        for entry in entries {
            crate::outbox_repo::insert_outbox_in_tx(&mut tx, entry).await?;
        }
        tx.commit().await.map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn save_many_with_outbox(
        &self,
        tasks: &[(&Task, SnapshotSource)],
        entries: &[OutboxEntry],
    ) -> PortResult<()> {
        // The reciprocal-edge guarantee of `save_many` AND the transactional-
        // outbox guarantee of `save_with_outbox`, in ONE transaction: a relation
        // edit's two task rows + the single outbound mutation it owes either all
        // commit or none do. Without this, a crash between the row write and the
        // enqueue would leave the relation permanently unsynced (relations have
        // no dirty-detection backstop). BEGIN IMMEDIATE grabs the writer lock
        // once for the whole unit; the outbox repo shares the same pool, so its
        // insert slots straight into this transaction.
        let mut tx = self
            .db
            .writes
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_sqlx_err)?;
        for (t, source) in tasks {
            write_task_in_tx(&mut tx, t, *source).await?;
        }
        for entry in entries {
            crate::outbox_repo::insert_outbox_in_tx(&mut tx, entry).await?;
        }
        tx.commit().await.map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn get(&self, id: TaskId) -> PortResult<Task> {
        let row = sqlx::query(&format!("SELECT {TASK_COLS} FROM tasks WHERE id = ?"))
            .bind(id.to_string())
            .fetch_optional(&self.db.reads)
            .await
            .map_err(map_sqlx_err)?
            .ok_or_else(|| PortError::NotFound(format!("task {id}")))?;
        let mut task = row_to_task(&row)?;
        task.relations = load_relations(&self.db.reads, id).await?;
        task.synced_baseline = load_latest_baseline(&self.db.reads, id).await?;
        task.comments = load_comments(&self.db.reads, id).await?;
        Ok(task)
    }

    async fn list(&self, filter: TaskFilter) -> PortResult<Vec<Task>> {
        // Columns are `tasks.`-qualified so the optional `JOIN workspaces` (for
        // the active-workspace gate) can't make `id`/`created_at`/`status`/etc.
        // ambiguous. Column ORDER is unchanged, so `row_to_task` (index-based)
        // is unaffected.
        let cols = TASK_COLS
            .split(", ")
            .map(|c| format!("tasks.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut from = format!("SELECT {cols} FROM tasks");
        if filter.pollable_workspaces_only {
            from.push_str(" JOIN workspaces ON workspaces.id = tasks.workspace_id");
        }
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(from);
        qb.push(" WHERE 1=1");
        if filter.pollable_workspaces_only {
            // Poller gate (RFC 0004 D3): only `active`, project-attached
            // workspaces are polled. The `status = 'active'` half excludes
            // paused/archived/created/deleted (a future variant stays excluded
            // by the explicit equality); `project_id IS NOT NULL` excludes a
            // projectless workspace whose tasks could otherwise sit in the
            // stale-scan forever (never reconcilable/stampable). The tripwire
            // test pins both halves.
            qb.push(" AND workspaces.status = 'active' AND workspaces.project_id IS NOT NULL");
        }
        if let Some(w) = filter.workspace_id {
            qb.push(" AND tasks.workspace_id = ")
                .push_bind(w.to_string());
        }
        if let Some(r) = filter.repo_id {
            qb.push(" AND tasks.repo_instance_id = ")
                .push_bind(r.to_string());
        }
        if let Some(open) = filter.is_open {
            // Filter on the open/closed bit (RFC 0004 D1). open ⇒
            // open|reopened, closed ⇒ completed|not_planned. `None` returns
            // both — there is no longer an implicit "hide archived" default
            // (the Archived status is gone; a closed task is just closed).
            if open {
                qb.push(" AND tasks.lifecycle IN ('open', 'reopened')");
            } else {
                qb.push(" AND tasks.lifecycle IN ('completed', 'not_planned')");
            }
        }
        if let Some(s) = filter.sync_state {
            qb.push(" AND tasks.sync_state = ")
                .push_bind(enum_to_str(&s)?);
        }
        if filter.has_project_item_id {
            qb.push(" AND tasks.project_item_id IS NOT NULL");
        }
        if let Some(ts) = filter.synced_at_lt {
            // Stale-scan: never-observed (NULL) or older than the threshold.
            qb.push(" AND (tasks.synced_at IS NULL OR tasks.synced_at < ")
                .push_bind(ts.into_inner())
                .push(")");
        }
        // In stale-scan mode order oldest-observed first (SQLite sorts NULLs
        // first in ASC — exactly the "never observed first" we want under the
        // LIMIT); otherwise keep the default creation order.
        if filter.synced_at_lt.is_some() {
            qb.push(" ORDER BY tasks.synced_at ASC");
        } else {
            qb.push(" ORDER BY tasks.created_at");
        }
        if let Some(limit) = filter.limit {
            qb.push(" LIMIT ").push_bind(limit as i64);
        }

        let rows = qb
            .build()
            .fetch_all(&self.db.reads)
            .await
            .map_err(map_sqlx_err)?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut task = row_to_task(row)?;
            task.relations = load_relations(&self.db.reads, task.id).await?;
            task.synced_baseline = load_latest_baseline(&self.db.reads, task.id).await?;
            out.push(task);
        }
        Ok(out)
    }

    async fn find_by_hash(&self, hash: &str) -> PortResult<Option<Task>> {
        if hash.is_empty() {
            return Ok(None);
        }
        let row = sqlx::query(&format!("SELECT {TASK_COLS} FROM tasks WHERE hash = ?"))
            .bind(hash)
            .fetch_optional(&self.db.reads)
            .await
            .map_err(map_sqlx_err)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut task = row_to_task(&row)?;
        task.relations = load_relations(&self.db.reads, task.id).await?;
        task.synced_baseline = load_latest_baseline(&self.db.reads, task.id).await?;
        task.comments = load_comments(&self.db.reads, task.id).await?;
        Ok(Some(task))
    }

    async fn find_by_remote(
        &self,
        filing_repo_id: RepoOriginId,
        provider: &str,
        remote_id: &str,
    ) -> PortResult<Option<Task>> {
        // RFC 0005 §D4: filing_repo_id is now in ORIGIN id space; the migration
        // (step 7a) backfilled every remote-backed row, so the COALESCE fallback
        // to logical repo_id is no longer needed. Look up by filing_repo_id alone.
        let row = sqlx::query(&format!(
            "SELECT {TASK_COLS} FROM tasks WHERE filing_repo_id = ? AND remote_provider = ? AND remote_id = ?"
        ))
        .bind(filing_repo_id.to_string())
        .bind(provider)
        .bind(remote_id)
        .fetch_optional(&self.db.reads)
        .await
        .map_err(map_sqlx_err)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut task = row_to_task(&row)?;
        task.relations = load_relations(&self.db.reads, task.id).await?;
        task.synced_baseline = load_latest_baseline(&self.db.reads, task.id).await?;
        task.comments = load_comments(&self.db.reads, task.id).await?;
        Ok(Some(task))
    }

    async fn replace_comments(
        &self,
        task_id: TaskId,
        comments: &[RemoteComment],
    ) -> PortResult<()> {
        let mut tx = self
            .db
            .writes
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_sqlx_err)?;
        // Replace only the synced (remote-backed) comments; pending local
        // comments (remote_comment_id = '') are left intact.
        sqlx::query("DELETE FROM task_comments WHERE task_id = ? AND remote_comment_id != ''")
            .bind(task_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;
        for c in comments {
            sqlx::query(
                "INSERT INTO task_comments (id, task_id, remote_comment_id, author, body, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(task_id.to_string())
            .bind(&c.remote_id)
            .bind(&c.author)
            .bind(&c.body)
            .bind(c.created_at.into_inner())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;
        }
        tx.commit().await.map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn add_pending_comment(
        &self,
        task_id: TaskId,
        author: &str,
        body: &str,
        created_at: Timestamp,
    ) -> PortResult<()> {
        // '' sentinel marks this as pending (no remote id yet). No snapshot.
        sqlx::query(
            "INSERT INTO task_comments (id, task_id, remote_comment_id, author, body, created_at) \
             VALUES (?, ?, '', ?, ?, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(task_id.to_string())
        .bind(author)
        .bind(body)
        .bind(created_at.into_inner())
        .execute(&self.db.writes)
        .await
        .map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn mark_comments_pushed(
        &self,
        task_id: TaskId,
        drained_local_ids: &[String],
        pushed: &[RemoteComment],
    ) -> PortResult<()> {
        let mut tx = self
            .db
            .writes
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_sqlx_err)?;
        // Identity-aware drain: delete only the rows whose surrogate id was
        // actually pushed. A pending comment added concurrently between push
        // reading the task and this commit keeps its `''` sentinel and lands
        // in the next drain rather than being silently destroyed.
        for local_id in drained_local_ids {
            sqlx::query(
                "DELETE FROM task_comments WHERE task_id = ? AND id = ? AND remote_comment_id = ''",
            )
            .bind(task_id.to_string())
            .bind(local_id)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;
        }
        for c in pushed {
            sqlx::query(
                "INSERT INTO task_comments (id, task_id, remote_comment_id, author, body, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(task_id.to_string())
            .bind(&c.remote_id)
            .bind(&c.author)
            .bind(&c.body)
            .bind(c.created_at.into_inner())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_err)?;
        }
        tx.commit().await.map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn pending_comment_counts(
        &self,
        workspace_id: WorkspaceId,
    ) -> PortResult<std::collections::HashMap<TaskId, usize>> {
        let rows = sqlx::query(
            "SELECT c.task_id AS task_id, COUNT(*) AS n \
             FROM task_comments c JOIN tasks t ON t.id = c.task_id \
             WHERE t.workspace_id = ? AND c.remote_comment_id = '' \
             GROUP BY c.task_id",
        )
        .bind(workspace_id.to_string())
        .fetch_all(&self.db.reads)
        .await
        .map_err(map_sqlx_err)?;

        let mut out = std::collections::HashMap::new();
        for row in rows {
            let task_id: String = row.try_get("task_id").map_err(map_sqlx_err)?;
            let n: i64 = row.try_get("n").map_err(map_sqlx_err)?;
            let task_id: TaskId = task_id
                .parse()
                .map_err(|e: domain_core::IdParseError| PortError::Backend(e.to_string()))?;
            out.insert(task_id, n as usize);
        }
        Ok(out)
    }

    async fn cache_project_status(
        &self,
        task_id: TaskId,
        option_id: Option<String>,
    ) -> PortResult<()> {
        // Targeted single-column write (#56, thread r3325841752): the cached
        // project-board status is orthogonal to the task aggregate, so it must
        // NOT go through `write_task_in_tx` — no version bump, no snapshot, no
        // `sync_state` change, and crucially no whole-row overwrite that would
        // clobber a concurrent CLI edit to title/body/status. A zero-row match
        // (task absent) is a benign no-op: the statement simply updates nothing.
        sqlx::query("UPDATE tasks SET project_status_option_id = ? WHERE id = ?")
            .bind(option_id)
            .bind(task_id.to_string())
            .execute(&self.db.writes)
            .await
            .map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn mark_remote_dirty(&self, task_id: TaskId) -> PortResult<()> {
        // Targeted, CONDITIONAL single-column write (#208): flip Synced →
        // DirtyRemote only. The `WHERE sync_state = synced` guard means a
        // concurrent CLI edit that already moved the row to DirtyLocal/Conflict/
        // Staged is never clobbered, and no other column, version, or snapshot
        // is touched. A zero-row match (task absent or not Synced) is a benign
        // no-op. enum_to_str keeps the stored strings rename-safe.
        sqlx::query("UPDATE tasks SET sync_state = ? WHERE id = ? AND sync_state = ?")
            .bind(enum_to_str(&SyncState::DirtyRemote)?)
            .bind(task_id.to_string())
            .bind(enum_to_str(&SyncState::Synced)?)
            .execute(&self.db.writes)
            .await
            .map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn cache_remote_node_id(&self, task_id: TaskId, node_id: String) -> PortResult<()> {
        // Targeted single-column write — same rationale as `cache_project_status`
        // above: `sync pull`'s Noop branch makes no aggregate write, so routing
        // this through `write_task_in_tx` would bump the version, append a
        // snapshot, and risk clobbering a concurrent CLI edit. `remote_node_id`
        // is excluded from the dirty diff, so a bare column update is safe.
        //
        // `AND remote_id IS NOT NULL` enforces the invariant that a node id only
        // exists alongside a remote: a remote-less (local-only / draft) row is a
        // no-op, so we never strand a dangling `remote_node_id`. This mirrors the
        // in-memory fixture, which no-ops when `task.remote` is None. A zero-row
        // match (task absent OR remote-less) is benign.
        sqlx::query("UPDATE tasks SET remote_node_id = ? WHERE id = ? AND remote_id IS NOT NULL")
            .bind(node_id)
            .bind(task_id.to_string())
            .execute(&self.db.writes)
            .await
            .map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn cache_synced_at(
        &self,
        task_id: TaskId,
        synced_at: Timestamp,
        _source: SyncedSource,
    ) -> PortResult<()> {
        // Targeted single-column write (RFC 0004 D3) — same family as
        // `cache_project_status` / `cache_remote_node_id`: stamp ONLY the
        // `synced_at` cache column, never the whole row, so observing the
        // remote can't clobber a concurrent CLI edit or perturb dirty
        // detection. `source` is not persisted (no column) — it exists only so
        // `mark_synced` can assert the call site. Zero-row match is benign.
        sqlx::query("UPDATE tasks SET synced_at = ? WHERE id = ?")
            .bind(synced_at.into_inner())
            .bind(task_id.to_string())
            .execute(&self.db.writes)
            .await
            .map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn delete(&self, id: TaskId) -> PortResult<()> {
        sqlx::query("DELETE FROM tasks WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.db.writes)
            .await
            .map_err(map_sqlx_err)?;
        Ok(())
    }
}

/// Apply one task's full write set — version bump, task upsert, snapshot
/// append, relation replace, and remote-mapping mirror — inside an existing
/// transaction. Shared by [`SqliteTaskRepository::save`] (single task) and
/// `save_many` (a batch) so both get identical persistence semantics; the
/// caller owns the surrounding `BEGIN`/`COMMIT`.
async fn write_task_in_tx(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    t: &Task,
    source: SnapshotSource,
) -> PortResult<()> {
    // Assign the next monotonic version for this task. COALESCE handles
    // the first-snapshot case (no rows yet → version 1).
    let next_version: i64 =
        sqlx::query("SELECT COALESCE(MAX(version), 0) + 1 FROM task_snapshots WHERE task_id = ?")
            .bind(t.id.to_string())
            .fetch_one(&mut **tx)
            .await
            .map_err(map_sqlx_err)?
            .try_get(0)
            .map_err(map_sqlx_err)?;

    sqlx::query(
        r#"
        INSERT INTO tasks (id, workspace_id, repo_instance_id, filing_repo_id, title, body, status, lifecycle, sync_state, priority, assignees_json, remote_provider, remote_id, remote_node_id, project_item_id, project_status_option_id, synced_at, hash, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            workspace_id = excluded.workspace_id,
            repo_instance_id = excluded.repo_instance_id,
            filing_repo_id = excluded.filing_repo_id,
            title = excluded.title,
            body = excluded.body,
            status = excluded.status,
            lifecycle = excluded.lifecycle,
            sync_state = excluded.sync_state,
            priority = excluded.priority,
            assignees_json = excluded.assignees_json,
            remote_provider = excluded.remote_provider,
            remote_id = excluded.remote_id,
            remote_node_id = excluded.remote_node_id,
            project_item_id = excluded.project_item_id,
            -- Stage 8 (#39): the cached project-board status must persist on
            -- upsert too — the poller writes it via `save`, which always hits
            -- the DO UPDATE half (the row already exists). Omitting this clause
            -- is the silent-never-persists bug class.
            project_status_option_id = excluded.project_status_option_id,
            -- RFC 0004 D3: `synced_at` is a write-through cache on a separate
            -- axis, stamped ONLY by the targeted `cache_synced_at`. It is
            -- deliberately NOT in this DO UPDATE set: a whole-row save carries a
            -- possibly-stale in-memory `synced_at` (e.g. loaded before the
            -- poller observed the remote), so updating it here would clobber a
            -- fresher stamp with an older value. New rows still get it via the
            -- INSERT column above (NULL = never observed).
            hash = excluded.hash,
            updated_at = excluded.updated_at
        "#,
    )
    .bind(t.id.to_string())
    .bind(t.workspace_id.to_string())
    .bind(t.repo_id.map(|r| r.to_string()))
    .bind(t.filing_repo_id.map(|r| r.to_string()))
    .bind(&t.title)
    .bind(&t.body)
    // Legacy `status` column (RFC 0004 D1): still written to satisfy its
    // NOT NULL CHECK, derived from the canonical `lifecycle`. No longer read.
    .bind(legacy_status_str(t.lifecycle))
    .bind(enum_to_str(&t.lifecycle)?)
    .bind(enum_to_str(&t.sync)?)
    .bind(enum_to_str(&t.priority)?)
    .bind(json_to_string(&t.assignees)?)
    .bind(t.remote.as_ref().map(|r| r.provider.clone()))
    .bind(t.remote.as_ref().map(|r| r.remote_id.clone()))
    .bind(t.remote.as_ref().and_then(|r| r.node_id.clone()))
    .bind(t.project_item_id.as_deref())
    .bind(t.project_status_option_id.as_deref())
    .bind(t.synced_at.map(|ts| ts.into_inner()))
    .bind(&t.hash)
    .bind(t.created_at.into_inner())
    .bind(t.updated_at.into_inner())
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx_err)?;

    // Append the snapshot row after the task upsert so the FK constraint
    // (task_snapshots.task_id → tasks.id) is satisfied.
    // `repo_id_recorded` is always 1 here (fresh snapshots record the binding;
    // see `Task::snapshot_view`). `filing_repo_id` (RFC 0002 #118) is captured
    // for history/audit; it has no `_recorded` companion because rollback never
    // restores it.
    sqlx::query(
        r#"
        INSERT INTO task_snapshots (task_id, version, title, body, status, lifecycle, sync_state, priority, assignees_json, remote_provider, remote_id, repo_id, repo_id_recorded, filing_repo_id, source, captured_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?)
        "#,
    )
    .bind(t.id.to_string())
    .bind(next_version)
    .bind(&t.title)
    .bind(&t.body)
    // Legacy `status` column derived from `lifecycle` (RFC 0004 D1); kept to
    // satisfy its NOT NULL CHECK, no longer read.
    .bind(legacy_status_str(t.lifecycle))
    .bind(enum_to_str(&t.lifecycle)?)
    .bind(enum_to_str(&t.sync)?)
    .bind(enum_to_str(&t.priority)?)
    .bind(json_to_string(&t.assignees)?)
    .bind(t.remote.as_ref().map(|r| r.provider.clone()))
    .bind(t.remote.as_ref().map(|r| r.remote_id.clone()))
    .bind(t.repo_id.map(|r| r.to_string()))
    .bind(t.filing_repo_id.map(|r| r.to_string()))
    .bind(enum_to_str(&source)?)
    .bind(Timestamp::now().into_inner())
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx_err)?;

    sqlx::query("DELETE FROM task_relations WHERE task_id = ?")
        .bind(t.id.to_string())
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx_err)?;

    for r in &t.relations {
        sqlx::query("INSERT INTO task_relations (task_id, kind, other_task_id) VALUES (?, ?, ?)")
            .bind(t.id.to_string())
            .bind(enum_to_str(&r.kind)?)
            .bind(r.other.to_string())
            .execute(&mut **tx)
            .await
            .map_err(map_sqlx_err)?;
    }

    // Mirror remote ref into the remote_mappings table for unique-index
    // protection. The unique key is (filing_repo_id, provider, remote_id)
    // (RFC 0002 D6) — the issue lives in the filing repo, and remote issue
    // numbers are only unique within a repo.
    if let Some(remote) = &t.remote {
        sqlx::query(
            r#"
            INSERT INTO remote_mappings (task_id, filing_repo_id, provider, remote_id, last_synced_at)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(task_id) DO UPDATE SET
                filing_repo_id = excluded.filing_repo_id,
                provider = excluded.provider,
                remote_id = excluded.remote_id,
                last_synced_at = excluded.last_synced_at
            "#,
        )
        .bind(t.id.to_string())
        // RFC 0005 §D4: filing_repo_id is now in ORIGIN id space; the fallback
        // to logical repo_id is removed — step 7a of the migration guarantees
        // every remote-backed task has a populated filing_repo_id. Empty-string
        // sentinel for a repo-less remote task keeps the UNIQUE key well-defined
        // (NULLs would dedupe as distinct).
        .bind(t.filing_repo_id.map(|r| r.to_string()).unwrap_or_default())
        .bind(&remote.provider)
        .bind(&remote.remote_id)
        .bind(t.updated_at.into_inner())
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx_err)?;
    } else {
        sqlx::query("DELETE FROM remote_mappings WHERE task_id = ?")
            .bind(t.id.to_string())
            .execute(&mut **tx)
            .await
            .map_err(map_sqlx_err)?;
    }

    Ok(())
}

/// Map the canonical [`Lifecycle`] back to a value in the legacy `status`
/// column's CHECK set (`open|in_progress|blocked|done|archived`). The column is
/// no longer read (RFC 0004 D1), but it is NOT NULL, so writes must still
/// supply a valid legacy value until a future migration retires the column.
fn legacy_status_str(lifecycle: Lifecycle) -> &'static str {
    match lifecycle {
        Lifecycle::Open | Lifecycle::Reopened => "open",
        Lifecycle::Completed => "done",
        Lifecycle::NotPlanned => "archived",
    }
}

fn row_to_task(row: &sqlx::sqlite::SqliteRow) -> PortResult<Task> {
    let id_str: String = row.try_get("id").map_err(map_sqlx_err)?;
    let workspace_id_str: String = row.try_get("workspace_id").map_err(map_sqlx_err)?;
    let repo_id_str: Option<String> = row.try_get("repo_instance_id").map_err(map_sqlx_err)?;
    let title: String = row.try_get("title").map_err(map_sqlx_err)?;
    let body: String = row.try_get("body").map_err(map_sqlx_err)?;
    // RFC 0004 D1: read the canonical `lifecycle`; legacy `status` is no longer read.
    let lifecycle: String = row.try_get("lifecycle").map_err(map_sqlx_err)?;
    let sync_state: String = row.try_get("sync_state").map_err(map_sqlx_err)?;
    let priority: String = row.try_get("priority").map_err(map_sqlx_err)?;
    let assignees_json: String = row.try_get("assignees_json").map_err(map_sqlx_err)?;
    let remote_provider: Option<String> = row.try_get("remote_provider").map_err(map_sqlx_err)?;
    let remote_id: Option<String> = row.try_get("remote_id").map_err(map_sqlx_err)?;
    let remote_node_id: Option<String> = row.try_get("remote_node_id").map_err(map_sqlx_err)?;
    let project_item_id: Option<String> = row.try_get("project_item_id").map_err(map_sqlx_err)?;
    let project_status_option_id: Option<String> = row
        .try_get("project_status_option_id")
        .map_err(map_sqlx_err)?;
    let hash: String = row.try_get("hash").map_err(map_sqlx_err)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(map_sqlx_err)?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at").map_err(map_sqlx_err)?;
    // RFC 0004 D3: write-through "remote last observed" stamp. NULL = never observed.
    let synced_at: Option<DateTime<Utc>> = row.try_get("synced_at").map_err(map_sqlx_err)?;

    let repo_id = repo_id_str
        .as_deref()
        .map(|s| parse_uuid::<RepoId>("repo_id", s))
        .transpose()?;

    // RFC 0002 filing repo (internal, #116). NULL until resolved at promote.
    let filing_repo_id = row
        .try_get::<Option<String>, _>("filing_repo_id")
        .map_err(map_sqlx_err)?
        .as_deref()
        .map(|s| parse_uuid::<RepoId>("filing_repo_id", s))
        .transpose()?;

    let remote = match (remote_provider, remote_id) {
        (Some(provider), Some(remote_id)) => Some(RemoteRef {
            provider,
            remote_id,
            node_id: remote_node_id,
        }),
        _ => None,
    };

    Ok(Task {
        id: parse_uuid::<TaskId>("task_id", &id_str)?,
        workspace_id: parse_uuid::<WorkspaceId>("workspace_id", &workspace_id_str)?,
        repo_id,
        filing_repo_id,
        title,
        body,
        lifecycle: enum_from_str::<Lifecycle>("task lifecycle", &lifecycle)?,
        sync: enum_from_str::<SyncState>("task sync_state", &sync_state)?,
        priority: enum_from_str::<Priority>("priority", &priority)?,
        assignees: json_from_string::<Vec<String>>("assignees", &assignees_json)?,
        remote,
        relations: Vec::new(),
        comments: Vec::new(),
        project_item_id,
        project_status_option_id,
        synced_at: synced_at.map(Timestamp::from_utc),
        hash,
        synced_baseline: None,
        created_at: Timestamp::from_utc(created_at),
        updated_at: Timestamp::from_utc(updated_at),
    })
}

async fn load_relations(pool: &SqlitePool, task_id: TaskId) -> PortResult<Vec<TaskRelation>> {
    let rows = sqlx::query(
        "SELECT kind, other_task_id FROM task_relations WHERE task_id = ? ORDER BY kind, other_task_id",
    )
    .bind(task_id.to_string())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_err)?;

    rows.iter()
        .map(|row| {
            let kind: String = row.try_get("kind").map_err(map_sqlx_err)?;
            let other: String = row.try_get("other_task_id").map_err(map_sqlx_err)?;
            Ok(TaskRelation {
                kind: enum_from_str::<RelationKind>("relation kind", &kind)?,
                other: parse_uuid::<TaskId>("task_id", &other)?,
            })
        })
        .collect()
}

async fn load_comments(pool: &SqlitePool, task_id: TaskId) -> PortResult<Vec<TaskComment>> {
    let rows = sqlx::query(
        "SELECT id, remote_comment_id, author, body, created_at FROM task_comments \
         WHERE task_id = ? ORDER BY created_at, id",
    )
    .bind(task_id.to_string())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_err)?;

    rows.iter()
        .map(|row| {
            let id: String = row.try_get("id").map_err(map_sqlx_err)?;
            let remote_comment_id: String =
                row.try_get("remote_comment_id").map_err(map_sqlx_err)?;
            let author: String = row.try_get("author").map_err(map_sqlx_err)?;
            let body: String = row.try_get("body").map_err(map_sqlx_err)?;
            let created_at: DateTime<Utc> = row.try_get("created_at").map_err(map_sqlx_err)?;
            Ok(TaskComment {
                local_id: Some(id),
                // '' sentinel ⇒ a pending local comment with no remote id yet.
                remote_id: (!remote_comment_id.is_empty()).then_some(remote_comment_id),
                author,
                body,
                created_at: Timestamp::from_utc(created_at),
            })
        })
        .collect()
}

async fn load_latest_baseline(
    pool: &SqlitePool,
    task_id: TaskId,
) -> PortResult<Option<TaskSnapshot>> {
    let row = sqlx::query(
        r#"
        SELECT version, title, body, lifecycle, sync_state, priority,
               assignees_json, remote_provider, remote_id, repo_id, repo_id_recorded,
               filing_repo_id, source, captured_at
        FROM task_snapshots
        WHERE task_id = ?
          -- `link` is baseline-eligible only on the verified-relink path
          -- (task stays Synced); bare links flip to Conflict and explicitly
          -- do NOT establish remote alignment.
          AND (
              source IN ('promote', 'push', 'pull', 'conflict_resolve')
              OR (source = 'link' AND sync_state != 'conflict')
          )
        ORDER BY version DESC
        LIMIT 1
        "#,
    )
    .bind(task_id.to_string())
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_err)?;

    let Some(row) = row else {
        return Ok(None);
    };

    let version: i64 = row.try_get("version").map_err(map_sqlx_err)?;
    let title: String = row.try_get("title").map_err(map_sqlx_err)?;
    let body: String = row.try_get("body").map_err(map_sqlx_err)?;
    // RFC 0004 D1: read the canonical `lifecycle`; legacy `status` is no longer read.
    let lifecycle: String = row.try_get("lifecycle").map_err(map_sqlx_err)?;
    let sync_state: String = row.try_get("sync_state").map_err(map_sqlx_err)?;
    let priority: String = row.try_get("priority").map_err(map_sqlx_err)?;
    let assignees_json: String = row.try_get("assignees_json").map_err(map_sqlx_err)?;
    let remote_provider: Option<String> = row.try_get("remote_provider").map_err(map_sqlx_err)?;
    let remote_id: Option<String> = row.try_get("remote_id").map_err(map_sqlx_err)?;
    let repo_id_raw: Option<String> = row.try_get("repo_id").map_err(map_sqlx_err)?;
    let repo_id_recorded_raw: i64 = row.try_get("repo_id_recorded").map_err(map_sqlx_err)?;
    let filing_repo_id_raw: Option<String> = row.try_get("filing_repo_id").map_err(map_sqlx_err)?;
    let source: String = row.try_get("source").map_err(map_sqlx_err)?;
    let captured_at: DateTime<Utc> = row.try_get("captured_at").map_err(map_sqlx_err)?;

    let remote = match (remote_provider, remote_id) {
        (Some(provider), Some(remote_id)) => Some(RemoteRef::new(provider, remote_id)),
        _ => None,
    };
    let repo_id = repo_id_raw
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<RepoId>())
        .transpose()
        .map_err(|e: domain_core::IdParseError| PortError::Backend(e.to_string()))?;
    // RFC 0002 #118: filing repo carried for history/audit. Second read path
    // (baseline hydration). Pre-column rows read back None.
    let filing_repo_id = filing_repo_id_raw
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<RepoId>())
        .transpose()
        .map_err(|e: domain_core::IdParseError| PortError::Backend(e.to_string()))?;

    Ok(Some(TaskSnapshot {
        task_id,
        version: version as u64,
        title,
        body,
        lifecycle: enum_from_str::<Lifecycle>("task lifecycle", &lifecycle)?,
        sync_state: enum_from_str::<SyncState>("task sync_state", &sync_state)?,
        priority: enum_from_str::<Priority>("priority", &priority)?,
        assignees: json_from_string::<Vec<String>>("assignees", &assignees_json)?,
        remote,
        repo_id,
        repo_id_recorded: repo_id_recorded_raw != 0,
        filing_repo_id,
        source: enum_from_str::<SnapshotSource>("snapshot source", &source)?,
        captured_at: Timestamp::from_utc(captured_at),
    }))
}
