use std::path::PathBuf;
use std::sync::Arc;

use domain_repo::RepoBinding;
use domain_task::{Priority, RelationKind, RemoteRef, SnapshotSource, Task};
use domain_workspace::{Workspace, WorkspaceName};
use infra_sqlite::{
    SqliteRepoBindingRepository, SqliteTaskRepository, SqliteWorkspaceRepository, open_from_path,
    backfill_empty_repo_names,
};
use ports::{RepoBindingRepository, TaskFilter, TaskRepository, WorkspaceRepository};
use tempfile::TempDir;

/// Build a fresh DB inside a TempDir. Caller MUST keep the TempDir alive for
/// the duration of the test — dropping it deletes the database file.
async fn setup() -> (
    TempDir,
    SqliteWorkspaceRepository,
    SqliteRepoBindingRepository,
    SqliteTaskRepository,
) {
    let (dir, _db, ws, rb, ts) = setup_with_db().await;
    (dir, ws, rb, ts)
}

async fn setup_with_db() -> (
    TempDir,
    infra_sqlite::Db,
    SqliteWorkspaceRepository,
    SqliteRepoBindingRepository,
    SqliteTaskRepository,
) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("repo-link.db");
    let db = open_from_path(&db_path).await.expect("open db");
    (
        dir,
        db.clone(),
        SqliteWorkspaceRepository::new(db.clone()),
        SqliteRepoBindingRepository::new(db.clone()),
        SqliteTaskRepository::new(db),
    )
}

#[tokio::test]
async fn workspace_roundtrip() {
    let (_dir, ws, _rb, _ts) = setup().await;
    let w = Workspace::new(WorkspaceName::new("scratch").unwrap(), Some("hi".into()), true);
    ws.save(&w).await.unwrap();
    let back = ws.get(w.id).await.unwrap();
    assert_eq!(back.name.as_str(), "scratch");
    assert_eq!(back.description.as_deref(), Some("hi"));
    let listed = ws.list(false).await.unwrap();
    assert_eq!(listed.len(), 1);
}

#[tokio::test]
async fn unique_workspace_name_enforced_by_db() {
    let (_dir, ws, _rb, _ts) = setup().await;
    let a = Workspace::new(WorkspaceName::new("dup").unwrap(), None, true);
    let b = Workspace::new(WorkspaceName::new("dup").unwrap(), None, true);
    ws.save(&a).await.unwrap();
    let err = ws.save(&b).await.expect_err("duplicate name should fail");
    let msg = format!("{err:?}").to_lowercase();
    assert!(msg.contains("unique") || msg.contains("conflict"), "got: {err:?}");
}

#[tokio::test]
async fn repo_with_worktrees_roundtrip() {
    let (_dir, ws, rb, _ts) = setup().await;
    let w = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
    ws.save(&w).await.unwrap();
    let mut binding = RepoBinding::new(
        w.id,
        "git@github.com:o/r.git".into(),
        "github.com/o/r".into(),
    )
    .unwrap();
    binding.link_worktree(PathBuf::from("/tmp/a"), Some("main".into()));
    binding.link_worktree(PathBuf::from("/tmp/b"), None);
    binding
        .mark_path_missing(std::path::Path::new("/tmp/b"))
        .unwrap();
    rb.save(&binding).await.unwrap();

    let back = rb.get(binding.id).await.unwrap();
    assert_eq!(back.worktrees.len(), 2);
    let by_path: std::collections::HashMap<_, _> = back
        .worktrees
        .iter()
        .map(|w| (w.path.display().to_string(), w))
        .collect();
    assert_eq!(by_path["/tmp/a"].branch.as_deref(), Some("main"));
    assert_eq!(by_path["/tmp/b"].status, domain_repo::LinkStatus::MissingPath);

    // Replace child collection: prune missing and resave.
    let mut updated = back;
    updated.prune_missing();
    rb.save(&updated).await.unwrap();
    let after = rb.get(binding.id).await.unwrap();
    assert_eq!(after.worktrees.len(), 1);
}

#[tokio::test]
async fn task_with_relations_and_remote_roundtrip() {
    let (_dir, ws, _rb, ts) = setup().await;
    let w = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
    ws.save(&w).await.unwrap();

    let mut other = Task::new_draft(w.id, None, "blocker".into()).unwrap();
    let mut task = Task::new_draft(w.id, None, "the work".into()).unwrap();
    task.set_priority(Priority::P1);
    task.assignees = vec!["alice".into(), "bob".into()];
    task.add_relation(RelationKind::BlockedBy, other.id);
    task.stage_for_sync().unwrap();
    task.promote_to_remote(RemoteRef {
        provider: "github".into(),
        remote_id: "o/r#42".into(),
    })
    .unwrap();
    other.archive().unwrap();

    ts.save(&other, SnapshotSource::LocalEdit).await.unwrap();
    ts.save(&task, SnapshotSource::LocalEdit).await.unwrap();

    let back = ts.get(task.id).await.unwrap();
    assert_eq!(back.priority, Priority::P1);
    assert_eq!(back.assignees, vec!["alice".to_string(), "bob".to_string()]);
    assert_eq!(back.relations.len(), 1);
    assert_eq!(back.relations[0].kind, RelationKind::BlockedBy);
    assert_eq!(back.relations[0].other, other.id);
    assert_eq!(
        back.remote.as_ref().map(|r| r.remote_id.as_str()),
        Some("o/r#42")
    );

    // Default list excludes archived.
    let live = ts.list(TaskFilter::default()).await.unwrap();
    assert_eq!(live.len(), 1);
    let all = ts
        .list(TaskFilter {
            include_archived: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
}

#[tokio::test]
async fn list_filters_compose() {
    let (_dir, ws, _rb, ts) = setup().await;
    let w1 = Workspace::new(WorkspaceName::new("w1").unwrap(), None, true);
    let w2 = Workspace::new(WorkspaceName::new("w2").unwrap(), None, true);
    ws.save(&w1).await.unwrap();
    ws.save(&w2).await.unwrap();

    for ws_id in [w1.id, w2.id] {
        for n in 0..2 {
            ts.save(&Task::new_draft(ws_id, None, format!("t{n}")).unwrap(), SnapshotSource::LocalEdit)
                .await
                .unwrap();
        }
    }

    let w1_tasks = ts
        .list(TaskFilter {
            workspace_id: Some(w1.id),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(w1_tasks.len(), 2);
    let all = ts.list(TaskFilter::default()).await.unwrap();
    assert_eq!(all.len(), 4);
}

#[tokio::test]
async fn deleting_workspace_cascades_to_tasks() {
    let (_dir, ws, _rb, ts) = setup().await;
    let w = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
    ws.save(&w).await.unwrap();
    let t = Task::new_draft(w.id, None, "t".into()).unwrap();
    ts.save(&t, SnapshotSource::LocalEdit).await.unwrap();

    ws.delete(w.id).await.unwrap();
    let after = ts.list(TaskFilter::default()).await.unwrap();
    assert!(after.is_empty(), "tasks should cascade with workspace delete");
}

// Wire a one-line use to demonstrate we can construct adapters via Arc<dyn Trait>.
#[tokio::test]
async fn adapters_satisfy_port_traits() {
    let (_dir, ws, rb, ts) = setup().await;
    let _ws: Arc<dyn WorkspaceRepository> = Arc::new(ws);
    let _rb: Arc<dyn RepoBindingRepository> = Arc::new(rb);
    let _ts: Arc<dyn TaskRepository> = Arc::new(ts);
}

// ---------------- Split-pool behaviour ----------------------------------

#[tokio::test]
async fn writer_pool_enables_wal_mode() {
    let (_dir, db, _ws, _rb, _ts) = setup_with_db().await;
    // Read the pragma through the reader pool — proves the WAL state set by
    // the writer's connect-time options is visible to readers.
    let mode: (String,) = sqlx::query_as("PRAGMA journal_mode;")
        .fetch_one(&db.reads)
        .await
        .unwrap();
    assert_eq!(mode.0.to_lowercase(), "wal");
}

#[tokio::test]
async fn concurrent_reads_during_active_write() {
    let (_dir, db, ws, _rb, _ts) = setup_with_db().await;
    // Seed a workspace so the read has something to return.
    let w = Workspace::new(WorkspaceName::new("scratch").unwrap(), None, true);
    ws.save(&w).await.unwrap();

    // Hold an open BEGIN IMMEDIATE transaction on the writer pool. With the
    // legacy single-pool setup this would block any concurrent read on the
    // same SQLite file; with WAL + split pools, reads sail past.
    let mut tx = db.writes.begin_with("BEGIN IMMEDIATE").await.unwrap();
    sqlx::query("INSERT INTO sync_events (at, payload_json) VALUES (?, ?)")
        .bind(chrono::Utc::now())
        .bind("{}")
        .execute(&mut *tx)
        .await
        .unwrap();

    // Read while the writer transaction is still uncommitted.
    let read = tokio::time::timeout(std::time::Duration::from_millis(500), async {
        let row: (String,) = sqlx::query_as("SELECT name FROM workspaces WHERE id = ?")
            .bind(w.id.to_string())
            .fetch_one(&db.reads)
            .await
            .unwrap();
        row.0
    })
    .await
    .expect("read should not block on the writer transaction");
    assert_eq!(read, "scratch");

    tx.commit().await.unwrap();
}

// ---------------- Phase B: backfill helper ----------------------------------

#[tokio::test]
async fn backfill_derives_name_for_empty_rows() {
    let (_dir, db, ws, _rb, _ts) = setup_with_db().await;

    // Seed a workspace so the FK constraint on repos is satisfied.
    let w = Workspace::new(WorkspaceName::new("bf-ws").unwrap(), None, true);
    ws.save(&w).await.unwrap();

    // Insert a repos row with name = '' directly, bypassing the repository
    // layer to simulate a row that predates the name column.
    sqlx::query(
        "INSERT INTO repos (id, workspace_id, remote_url, canonical_url, name, aliases, created_at, updated_at) \
         VALUES (?, ?, ?, ?, '', '[]', datetime('now'), datetime('now'))",
    )
    .bind("aaaaaaaa-0000-0000-0000-000000000001")
    .bind(w.id.to_string())
    .bind("git@github.com:org/myrepo.git")
    .bind("github.com/org/myrepo")
    .execute(&db.writes)
    .await
    .unwrap();

    // Run the backfill — should derive "myrepo" from the canonical URL.
    backfill_empty_repo_names(&db.writes).await.unwrap();

    let (name,): (String,) =
        sqlx::query_as("SELECT name FROM repos WHERE id = ?")
            .bind("aaaaaaaa-0000-0000-0000-000000000001")
            .fetch_one(&db.reads)
            .await
            .unwrap();

    assert_eq!(name, "myrepo");
}

#[tokio::test]
async fn backfill_is_idempotent_on_populated_rows() {
    let (_dir, db, ws, rb, _ts) = setup_with_db().await;
    let w = Workspace::new(WorkspaceName::new("bf-ws2").unwrap(), None, true);
    ws.save(&w).await.unwrap();

    let binding = RepoBinding::new(
        w.id,
        "git@github.com:org/proj.git".into(),
        "github.com/org/proj".into(),
    )
    .unwrap();
    rb.save(&binding).await.unwrap();

    // Running backfill again should be a no-op and not error.
    backfill_empty_repo_names(&db.writes).await.unwrap();

    let (name,): (String,) =
        sqlx::query_as("SELECT name FROM repos WHERE id = ?")
            .bind(binding.id.to_string())
            .fetch_one(&db.reads)
            .await
            .unwrap();

    assert_eq!(name, "proj");
}

#[tokio::test]
async fn concurrent_writes_serialize_without_busy_error() {
    let (dir, _db, ws, _rb, _ts) = setup_with_db().await;
    let dir_path = dir.path().to_path_buf();
    // Make sure the TempDir outlives the spawned tasks even though `setup`
    // owns it — clone the path and re-open through the same DB.
    let _keep = dir_path; // anchored for the test scope

    let ws = Arc::new(ws);
    let a = Workspace::new(WorkspaceName::new("a").unwrap(), None, true);
    let b = Workspace::new(WorkspaceName::new("b").unwrap(), None, true);

    let h1 = {
        let ws = ws.clone();
        let a = a.clone();
        tokio::spawn(async move { ws.save(&a).await })
    };
    let h2 = {
        let ws = ws.clone();
        let b = b.clone();
        tokio::spawn(async move { ws.save(&b).await })
    };

    h1.await.unwrap().expect("first write");
    h2.await.unwrap().expect("second write");

    let listed = ws.list(false).await.unwrap();
    assert_eq!(listed.len(), 2);
}
