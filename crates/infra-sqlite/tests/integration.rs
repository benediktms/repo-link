use std::path::PathBuf;
use std::sync::Arc;

use domain_repo::RepoBinding;
use domain_task::{Priority, RelationKind, RemoteRef, SnapshotSource, Task};
use domain_workspace::{Workspace, WorkspaceName};
use infra_sqlite::{
    SqliteRepoBindingRepository, SqliteTaskRepository, SqliteWorkspaceRepository,
    backfill_empty_repo_names, open_from_path,
};
use ports::{
    PortError, RemoteComment, RepoBindingRepository, TaskFilter, TaskRepository,
    WorkspaceRepository,
};
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
    let w = Workspace::new(
        WorkspaceName::new("scratch").unwrap(),
        Some("hi".into()),
        true,
    );
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
    assert!(
        msg.contains("unique") || msg.contains("conflict"),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn task_comments_roundtrip_and_replace() {
    let (_dir, ws, _rb, ts) = setup().await;
    let w = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
    ws.save(&w).await.unwrap();
    let t = Task::new_draft(w.id, None, "commented task".into()).unwrap();
    ts.save(&t, SnapshotSource::Created).await.unwrap();

    // Distinct created_at per comment so load order (chronological, like
    // GitHub) is deterministic — `Timestamp::now()` for all three would
    // collide at storage precision and fall back to the random surrogate id.
    let base = chrono::Utc::now();
    let mk = |id: &str, body: &str, secs: i64| RemoteComment {
        remote_id: id.into(),
        author: "octocat".into(),
        body: body.into(),
        created_at: domain_core::Timestamp::from_utc(base + chrono::Duration::seconds(secs)),
    };

    // First sync of two comments.
    ts.replace_comments(t.id, &[mk("1", "first", 0), mk("2", "second", 1)])
        .await
        .unwrap();
    let loaded = ts.get(t.id).await.unwrap();
    assert_eq!(loaded.comments.len(), 2);
    assert_eq!(loaded.comments[0].body, "first");
    assert_eq!(loaded.comments[0].remote_id.as_deref(), Some("1"));

    // Replacing the synced set with the latest remote view (now 3) reflects it
    // without duplicating the originals.
    ts.replace_comments(
        t.id,
        &[
            mk("1", "first", 0),
            mk("2", "second", 1),
            mk("3", "third", 2),
        ],
    )
    .await
    .unwrap();
    let loaded = ts.get(t.id).await.unwrap();
    assert_eq!(loaded.comments.len(), 3);
    assert_eq!(loaded.comments[2].remote_id.as_deref(), Some("3"));
}

#[tokio::test]
async fn pending_comments_add_and_drain_on_push() {
    let (_dir, ws, _rb, ts) = setup().await;
    let w = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
    ws.save(&w).await.unwrap();
    let t = Task::new_draft(w.id, None, "commented task".into()).unwrap();
    ts.save(&t, SnapshotSource::Created).await.unwrap();

    let base = chrono::Utc::now();
    let at = |secs: i64| domain_core::Timestamp::from_utc(base + chrono::Duration::seconds(secs));

    // One already-synced comment plus a locally-authored pending one.
    ts.replace_comments(
        t.id,
        &[RemoteComment {
            remote_id: "1".into(),
            author: "octocat".into(),
            body: "synced".into(),
            created_at: at(0),
        }],
    )
    .await
    .unwrap();
    ts.add_pending_comment(t.id, "me", "pending body", at(1))
        .await
        .unwrap();

    let loaded = ts.get(t.id).await.unwrap();
    assert_eq!(loaded.comments.len(), 2);
    let pending: Vec<_> = loaded
        .comments
        .iter()
        .filter(|c| c.remote_id.is_none())
        .collect();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].body, "pending body");
    let drained_id = pending[0]
        .local_id
        .clone()
        .expect("loaded comment has local_id");

    // Draining promotes the pending comment to synced — no duplicates, the
    // already-synced comment is untouched.
    ts.mark_comments_pushed(
        t.id,
        &[drained_id],
        &[RemoteComment {
            remote_id: "99".into(),
            author: "octocat".into(),
            body: "pending body".into(),
            created_at: at(1),
        }],
    )
    .await
    .unwrap();

    let loaded = ts.get(t.id).await.unwrap();
    assert_eq!(loaded.comments.len(), 2);
    assert!(loaded.comments.iter().all(|c| c.remote_id.is_some()));
    assert_eq!(loaded.comments[1].remote_id.as_deref(), Some("99"));
}

#[tokio::test]
async fn mark_comments_pushed_does_not_race_delete_concurrent_pending() {
    // Simulates the concurrency hazard CodeRabbit flagged: a second pending
    // comment lands between push reading the task and the drain commit. The
    // drain must remove only the rows it actually pushed, leaving the fresh
    // pending intact for the next push.
    let (_dir, ws, _rb, ts) = setup().await;
    let w = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
    ws.save(&w).await.unwrap();
    let t = Task::new_draft(w.id, None, "commented task".into()).unwrap();
    ts.save(&t, SnapshotSource::Created).await.unwrap();

    let base = chrono::Utc::now();
    let at = |secs: i64| domain_core::Timestamp::from_utc(base + chrono::Duration::seconds(secs));

    // Pending A — what push will drain.
    ts.add_pending_comment(t.id, "me", "A", at(0))
        .await
        .unwrap();
    let loaded = ts.get(t.id).await.unwrap();
    let a_local_id = loaded.comments[0].local_id.clone().unwrap();

    // Concurrent add — lands after push read the task, before drain.
    ts.add_pending_comment(t.id, "me", "B", at(1))
        .await
        .unwrap();

    // Drain only A.
    ts.mark_comments_pushed(
        t.id,
        &[a_local_id],
        &[RemoteComment {
            remote_id: "100".into(),
            author: "octocat".into(),
            body: "A".into(),
            created_at: at(0),
        }],
    )
    .await
    .unwrap();

    let loaded = ts.get(t.id).await.unwrap();
    assert_eq!(loaded.comments.len(), 2, "B must survive an A-only drain");
    let pending: Vec<_> = loaded
        .comments
        .iter()
        .filter(|c| c.remote_id.is_none())
        .collect();
    assert_eq!(pending.len(), 1, "B remains pending");
    assert_eq!(pending[0].body, "B");
    let synced: Vec<_> = loaded
        .comments
        .iter()
        .filter(|c| c.remote_id.is_some())
        .collect();
    assert_eq!(synced[0].remote_id.as_deref(), Some("100"));
}

#[tokio::test]
async fn remote_mapping_is_repo_scoped() {
    let (_dir, ws, rb, ts) = setup().await;
    let w = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
    ws.save(&w).await.unwrap();

    // Two bindings in the same workspace.
    let repo_a = RepoBinding::new(
        w.id,
        "git@github.com:o/a.git".into(),
        "github.com/o/a".into(),
    )
    .unwrap();
    let repo_b = RepoBinding::new(
        w.id,
        "git@github.com:o/b.git".into(),
        "github.com/o/b".into(),
    )
    .unwrap();
    rb.save(&repo_a).await.unwrap();
    rb.save(&repo_b).await.unwrap();

    // A task in `repo` mirroring github issue `num`.
    let mk = |repo_id, num: &str| {
        let mut t = Task::new_draft(w.id, Some(repo_id), format!("issue {num}")).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", num)).unwrap();
        t
    };

    // Same issue number (#1) in two different repos must both persist —
    // remote identity is repo-scoped, so they don't collide.
    ts.save(&mk(repo_a.id, "1"), SnapshotSource::Promote)
        .await
        .unwrap();
    ts.save(&mk(repo_b.id, "1"), SnapshotSource::Promote)
        .await
        .expect("repoB#1 must not collide with repoA#1");

    // But the same (repo, provider, remote_id) still conflicts.
    let err = ts
        .save(&mk(repo_a.id, "1"), SnapshotSource::Promote)
        .await
        .expect_err("duplicate remote in the same repo should conflict");
    let msg = format!("{err:?}").to_lowercase();
    assert!(
        msg.contains("unique") || msg.contains("conflict"),
        "got: {err:?}"
    );
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
    assert_eq!(
        by_path["/tmp/b"].status,
        domain_repo::LinkStatus::MissingPath
    );

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
    task.promote_to_remote(RemoteRef::new("github", "o/r#42"))
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
            ts.save(
                &Task::new_draft(ws_id, None, format!("t{n}")).unwrap(),
                SnapshotSource::LocalEdit,
            )
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
    assert!(
        after.is_empty(),
        "tasks should cascade with workspace delete"
    );
}

#[tokio::test]
async fn save_many_persists_both_sides_of_a_reciprocal_edge() {
    let (_dir, ws, _rb, ts) = setup().await;
    let w = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
    ws.save(&w).await.unwrap();

    // Both tasks already exist — `add_relation` only ever batches updates to
    // tasks it has already loaded, so each relation's FK target is present.
    let mut a = Task::new_draft(w.id, None, "A".into()).unwrap();
    let mut b = Task::new_draft(w.id, None, "B".into()).unwrap();
    ts.save(&a, SnapshotSource::Created).await.unwrap();
    ts.save(&b, SnapshotSource::Created).await.unwrap();

    // Now wire up the two reciprocal halves of a `blocked_by`/`blocks` pair
    // and persist both sides in one atomic batch.
    a.add_relation(RelationKind::BlockedBy, b.id);
    b.add_relation(RelationKind::Blocks, a.id);
    ts.save_many(&[
        (&a, SnapshotSource::LocalEdit),
        (&b, SnapshotSource::LocalEdit),
    ])
    .await
    .unwrap();

    let back_a = ts.get(a.id).await.unwrap();
    let back_b = ts.get(b.id).await.unwrap();
    assert_eq!(back_a.relations.len(), 1);
    assert_eq!(back_a.relations[0].kind, RelationKind::BlockedBy);
    assert_eq!(back_a.relations[0].other, b.id);
    assert_eq!(back_b.relations.len(), 1);
    assert_eq!(back_b.relations[0].kind, RelationKind::Blocks);
    assert_eq!(back_b.relations[0].other, a.id);
}

#[tokio::test]
async fn save_many_rolls_back_the_whole_batch_on_a_mid_batch_failure() {
    let (_dir, ws, _rb, ts) = setup().await;
    let w = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
    ws.save(&w).await.unwrap();

    // Two tasks claiming the SAME remote issue. The remote_mappings
    // (repo_id, provider, remote_id) UNIQUE index rejects the second task's
    // mirror insert, which must abort the entire batch — not just its own row.
    let mut a = Task::new_draft(w.id, None, "A".into()).unwrap();
    let mut b = Task::new_draft(w.id, None, "B".into()).unwrap();
    for t in [&mut a, &mut b] {
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", "o/r#1"))
            .unwrap();
    }

    ts.save_many(&[(&a, SnapshotSource::Promote), (&b, SnapshotSource::Promote)])
        .await
        .expect_err("duplicate remote mapping should fail the batch");

    // Atomicity: the first task in the batch must NOT have been committed.
    let got = ts.get(a.id).await;
    assert!(
        matches!(got, Err(PortError::NotFound(_))),
        "first task should have rolled back with the batch, got {got:?}"
    );
}

// ---------------- Transactional outbox (#54) ----------------------------

#[tokio::test]
async fn save_with_outbox_persists_task_snapshot_and_entries_in_one_tx() {
    use domain_sync::{OutboxEntry, OutboxMutation};
    use infra_sqlite::SqliteOutboxRepository;
    use ports::{OutboxRepository, TaskSnapshotRepository};

    let (_dir, db, ws, _rb, ts) = setup_with_db().await;
    let w = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
    ws.save(&w).await.unwrap();

    // An issue-backed mirror with a lifecycle change to push.
    let mut t = Task::new_draft(w.id, None, "atomic".into()).unwrap();
    t.stage_for_sync().unwrap();
    t.promote_to_remote(RemoteRef::new("github", "42")).unwrap();
    t.start().unwrap();

    let entries = vec![
        OutboxEntry::new(
            t.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "42".into(),
                title: Some("atomic".into()),
                body: None,
                closed: None,
            },
        ),
        OutboxEntry::new(
            t.id,
            OutboxMutation::SetProjectStatus {
                project_node_id: "PVT_x".into(),
                item_node_id: "PVTI_y".into(),
                status_field_id: "PVTSSF_z".into(),
                option_id: "opt12345".into(),
            },
        ),
    ];
    ts.save_with_outbox(&t, SnapshotSource::Promote, &entries)
        .await
        .unwrap();

    // Task row round-trips with the lifecycle change.
    let back = ts.get(t.id).await.unwrap();
    assert_eq!(back.status, domain_task::TaskStatus::InProgress);
    // Snapshot history recorded the write.
    let snaps = infra_sqlite::SqliteTaskSnapshotRepository::new(db.clone())
        .list(t.id)
        .await
        .unwrap();
    assert_eq!(snaps.len(), 1, "exactly one snapshot for the single write");

    // Both outbox rows round-trip, in order, all pending.
    let outbox = SqliteOutboxRepository::new(db);
    let pending = outbox.list_pending(t.id).await.unwrap();
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].mutation.kind(), "update_remote");
    assert_eq!(pending[1].mutation.kind(), "set_project_status");
}

#[tokio::test]
async fn save_with_outbox_empty_entries_behaves_like_save() {
    use infra_sqlite::SqliteOutboxRepository;
    use ports::OutboxRepository;

    let (_dir, db, ws, _rb, ts) = setup_with_db().await;
    let w = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
    ws.save(&w).await.unwrap();

    let t = Task::new_draft(w.id, None, "no entries".into()).unwrap();
    ts.save_with_outbox(&t, SnapshotSource::Created, &[])
        .await
        .unwrap();

    // Task persisted exactly as `save` would.
    let back = ts.get(t.id).await.unwrap();
    assert_eq!(back.title, "no entries");
    // No outbox rows written.
    let outbox = SqliteOutboxRepository::new(db);
    assert!(outbox.list_pending(t.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn save_with_outbox_rolls_back_task_and_entries_on_failure() {
    use domain_sync::{OutboxEntry, OutboxMutation};
    use infra_sqlite::SqliteOutboxRepository;
    use ports::OutboxRepository;

    let (_dir, db, ws, _rb, ts) = setup_with_db().await;
    let w = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
    ws.save(&w).await.unwrap();

    // Two entries sharing the SAME id forces a PRIMARY KEY violation on the
    // second outbox INSERT, aborting the whole transaction — the task write and
    // the first entry must roll back with it.
    let mut t = Task::new_draft(w.id, None, "rollback".into()).unwrap();
    t.start().unwrap();
    let shared = OutboxEntry::new(
        t.id,
        OutboxMutation::UpdateRemote {
            canonical_repo: "github.com/o/r".into(),
            remote_id: "1".into(),
            title: None,
            body: None,
            closed: None,
        },
    );
    let dup = OutboxEntry {
        id: shared.id, // duplicate primary key
        ..shared.clone()
    };

    ts.save_with_outbox(&t, SnapshotSource::Created, &[shared, dup])
        .await
        .expect_err("duplicate outbox entry id must abort the transaction");

    // Atomicity: neither the task nor any outbox row survived.
    let got = ts.get(t.id).await;
    assert!(
        matches!(got, Err(PortError::NotFound(_))),
        "task must have rolled back with the failed combined write, got {got:?}"
    );
    let outbox = SqliteOutboxRepository::new(db);
    assert!(
        outbox.list_pending(t.id).await.unwrap().is_empty(),
        "no outbox row may survive a rolled-back combined write"
    );
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

    let (name,): (String,) = sqlx::query_as("SELECT name FROM repos WHERE id = ?")
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

    let (name,): (String,) = sqlx::query_as("SELECT name FROM repos WHERE id = ?")
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

/// The CHECK constraint on `repos.aliases` must reject valid JSON that
/// isn't a JSON array. Without `json_type(...) = 'array'`, an object or
/// scalar would slip through and break Vec<String> hydration on load.
#[tokio::test]
async fn aliases_check_rejects_non_array_json() {
    let (_dir, db, ws, _rb, _ts) = setup_with_db().await;
    let w = Workspace::new(WorkspaceName::new("ws-check").unwrap(), None, true);
    ws.save(&w).await.unwrap();

    // Try to insert a repo row whose aliases is a valid JSON *object*
    // (not an array). The CHECK constraint must reject this.
    let result = sqlx::query(
        r#"
        INSERT INTO repos (id, workspace_id, remote_url, canonical_url, tracked_branch,
                           name, aliases, created_at, updated_at)
        VALUES (?, ?, ?, ?, NULL, ?, ?, ?, ?)
        "#,
    )
    .bind("c08c09c5-4ac2-4a43-96ea-d574a580fde5")
    .bind(w.id.to_string())
    .bind("git@example.com:o/r.git")
    .bind("example.com/o/r")
    .bind("r")
    .bind(r#"{"not": "an array"}"#)
    .bind(chrono::Utc::now())
    .bind(chrono::Utc::now())
    .execute(&db.writes)
    .await;

    let err = result.expect_err("inserting a JSON object as aliases must violate CHECK");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("check"),
        "expected a CHECK constraint failure, got: {msg}"
    );
}

// ---------- RFC 0001 Stage 3 — project + outbox round-trips ----------------

#[tokio::test]
async fn project_roundtrip_preserves_options_and_default_mapping() {
    use domain_core::{ProjectId, Timestamp};
    use domain_project::{Project, StatusMapping, StatusOption};
    use domain_task::TaskStatus;
    use infra_sqlite::SqliteProjectRepository;
    use ports::ProjectRepository;

    let (_dir, db, _ws, _rb, _ts) = setup_with_db().await;
    let projects = SqliteProjectRepository::new(db);
    let id = ProjectId::parse("PVT_kwHO_test_abc").unwrap();
    let saved = Project::new(
        id.clone(),
        "acme".into(),
        7,
        "Repo Link".into(),
        "PVTSSF_field".into(),
        vec![
            StatusOption {
                option_id: "o1".into(),
                name: "Backlog".into(),
                ordinal: 0,
            },
            StatusOption {
                option_id: "o2".into(),
                name: "Done".into(),
                ordinal: 1,
            },
            StatusOption {
                option_id: "o3".into(),
                name: "Triage".into(),
                ordinal: 2,
            },
        ],
        vec![
            StatusMapping {
                status: TaskStatus::Open,
                option_id: "o1".into(),
            },
            StatusMapping {
                status: TaskStatus::Done,
                option_id: "o2".into(),
            },
        ],
        false,
        Timestamp::now(),
    )
    .unwrap();

    projects.save(&saved).await.unwrap();
    let loaded = projects.get(id.clone()).await.unwrap();

    // Identity + scalar fields round-trip unchanged.
    assert_eq!(loaded.id.as_str(), id.as_str());
    assert_eq!(loaded.owner_login, "acme");
    assert_eq!(loaded.number, 7);
    assert_eq!(loaded.title, "Repo Link");
    assert_eq!(loaded.status_field_id, "PVTSSF_field");
    assert!(!loaded.archived);

    // Options come back in the order they were stored (sorted by ordinal).
    assert_eq!(loaded.status_options.len(), 3);
    let names: Vec<&str> = loaded
        .status_options
        .iter()
        .map(|o| o.name.as_str())
        .collect();
    assert_eq!(names, ["Backlog", "Done", "Triage"]);

    // The two real mappings round-trip; the "Triage" option stays unmapped
    // (no project_status_mappings row) exactly as it was saved.
    assert_eq!(loaded.option_id_for(TaskStatus::Open), Some("o1"));
    assert_eq!(loaded.option_id_for(TaskStatus::Done), Some("o2"));
    assert_eq!(loaded.option_id_for(TaskStatus::InProgress), None);
}

#[tokio::test]
async fn project_roundtrip_preserves_many_to_one_mapping() {
    // Regression for #80: the Stage 3 scalar `default_for` column could hold
    // at most one TaskStatus per option, so saving a Project where two
    // statuses share one option silently dropped the second mapping. The
    // dedicated `project_status_mappings` table stores both rows; this test
    // pins that they survive the round-trip.
    use domain_core::{ProjectId, Timestamp};
    use domain_project::{Project, StatusMapping, StatusOption};
    use domain_task::TaskStatus;
    use infra_sqlite::SqliteProjectRepository;
    use ports::ProjectRepository;

    let (_dir, db, _ws, _rb, _ts) = setup_with_db().await;
    let projects = SqliteProjectRepository::new(db);
    let id = ProjectId::parse("PVT_kwHO_many_to_one").unwrap();

    // A board with fewer columns than we have local statuses: Open AND
    // Blocked both map to "Backlog"; InProgress and Done map to "Done".
    let saved = Project::new(
        id.clone(),
        "acme".into(),
        9,
        "Tight Board".into(),
        "PVTSSF_field".into(),
        vec![
            StatusOption {
                option_id: "backlog".into(),
                name: "Backlog".into(),
                ordinal: 0,
            },
            StatusOption {
                option_id: "done".into(),
                name: "Done".into(),
                ordinal: 1,
            },
        ],
        vec![
            StatusMapping {
                status: TaskStatus::Open,
                option_id: "backlog".into(),
            },
            StatusMapping {
                status: TaskStatus::Blocked,
                option_id: "backlog".into(),
            },
            StatusMapping {
                status: TaskStatus::InProgress,
                option_id: "done".into(),
            },
            StatusMapping {
                status: TaskStatus::Done,
                option_id: "done".into(),
            },
        ],
        false,
        Timestamp::now(),
    )
    .unwrap();

    projects.save(&saved).await.unwrap();
    let loaded = projects.get(id).await.unwrap();

    // All four mappings round-trip — not just the first one per option.
    assert_eq!(loaded.status_mappings.len(), 4);
    assert_eq!(loaded.option_id_for(TaskStatus::Open), Some("backlog"));
    assert_eq!(loaded.option_id_for(TaskStatus::Blocked), Some("backlog"));
    assert_eq!(loaded.option_id_for(TaskStatus::InProgress), Some("done"));
    assert_eq!(loaded.option_id_for(TaskStatus::Done), Some("done"));
}

#[tokio::test]
async fn outbox_claim_next_eligible_claims_oldest_and_flips_to_inflight() {
    use domain_core::Timestamp;
    use domain_sync::{OutboxEntry, OutboxMutation, OutboxStatus};
    use infra_sqlite::SqliteOutboxRepository;
    use ports::OutboxRepository;

    let (_dir, db, _ws, _rb, ts) = setup_with_db().await;

    // We need a task row for the FK on outbox_entries.task_id. Make a
    // minimal local-only one — the outbox doesn't read task fields.
    let workspace = Workspace::new(WorkspaceName::new("wsx").unwrap(), None, true);
    SqliteWorkspaceRepository::new(db.clone())
        .save(&workspace)
        .await
        .unwrap();
    let task = Task::new_draft(workspace.id, None, "t1".into()).unwrap();
    ts.save(&task, SnapshotSource::Created).await.unwrap();

    let outbox = SqliteOutboxRepository::new(db);

    // Enqueue two entries for the same task. The claim must return the
    // first-inserted one first; the tail must NOT be claimable while the head
    // is inflight (per-task FIFO). No `sleep()` to stagger `enqueued_at`: the
    // claim tie-breaks on the implicit `rowid`, so insertion order wins even at
    // equal (second-granular) timestamps — this is the case the old sleep hack
    // papered over.
    let e1 = OutboxEntry::new(
        task.id,
        OutboxMutation::UpdateRemote {
            canonical_repo: "github.com/o/r".into(),
            remote_id: "1".into(),
            title: Some("new title".into()),
            body: None,
            closed: None,
        },
    );
    let e2 = OutboxEntry::new(
        task.id,
        OutboxMutation::SetProjectStatus {
            project_node_id: "PVT_kwHO_x".into(),
            item_node_id: "PVTI_y".into(),
            status_field_id: "PVTSSF_z".into(),
            option_id: "abc12345".into(),
        },
    );
    outbox.enqueue(&e1).await.unwrap();
    outbox.enqueue(&e2).await.unwrap();

    let pending = outbox.list_pending(task.id).await.unwrap();
    assert_eq!(pending.len(), 2);

    // Claim — should return e1 (older), now flipped to inflight.
    let claimed = outbox
        .claim_next_eligible(Timestamp::now())
        .await
        .unwrap()
        .expect("a pending entry");
    assert_eq!(claimed.id, e1.id);
    assert_eq!(claimed.status, OutboxStatus::Inflight);

    // The tail is blocked behind the inflight head: a second claim returns
    // nothing for this task even though e2 is pending + eligible.
    assert!(
        outbox
            .claim_next_eligible(Timestamp::now())
            .await
            .unwrap()
            .is_none(),
        "tail must not be claimable while the head is inflight"
    );

    // list_pending now sees only e2 (e1 is inflight, not pending).
    let pending = outbox.list_pending(task.id).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, e2.id);

    // Mark e1 succeeded; mark_failed e2 to exercise both paths.
    outbox.mark_succeeded(e1.id).await.unwrap();
    outbox.mark_failed(e2.id, "graphql 5xx").await.unwrap();
    // After mark_failed, e2 is no longer in `pending`.
    let pending = outbox.list_pending(task.id).await.unwrap();
    assert!(pending.is_empty());
}

#[tokio::test]
async fn outbox_requeue_orphaned_inflight_resets_to_pending() {
    use domain_core::Timestamp;
    use domain_sync::{OutboxEntry, OutboxMutation, OutboxStatus};
    use infra_sqlite::SqliteOutboxRepository;
    use ports::OutboxRepository;

    let (_dir, db, _ws, _rb, ts) = setup_with_db().await;
    let workspace = Workspace::new(WorkspaceName::new("wsr").unwrap(), None, true);
    SqliteWorkspaceRepository::new(db.clone())
        .save(&workspace)
        .await
        .unwrap();
    let task = Task::new_draft(workspace.id, None, "t1".into()).unwrap();
    ts.save(&task, SnapshotSource::Created).await.unwrap();

    let outbox = SqliteOutboxRepository::new(db);
    let e = OutboxEntry::new(
        task.id,
        OutboxMutation::UpdateRemote {
            canonical_repo: "github.com/o/r".into(),
            remote_id: "1".into(),
            title: None,
            body: None,
            closed: None,
        },
    );
    outbox.enqueue(&e).await.unwrap();

    // Claim it — now inflight. Simulate a crash by never resolving it.
    let claimed = outbox
        .claim_next_eligible(Timestamp::now())
        .await
        .unwrap()
        .expect("a pending entry");
    assert_eq!(claimed.status, OutboxStatus::Inflight);
    // While inflight, nothing is claimable.
    assert!(
        outbox
            .claim_next_eligible(Timestamp::now())
            .await
            .unwrap()
            .is_none()
    );

    // Startup recovery resets the orphaned inflight row back to pending.
    let reset = outbox.requeue_orphaned_inflight().await.unwrap();
    assert_eq!(reset, 1);
    let reclaimed = outbox
        .claim_next_eligible(Timestamp::now())
        .await
        .unwrap()
        .expect("reclaimed after requeue");
    assert_eq!(reclaimed.id, e.id);
}

#[tokio::test]
async fn workspace_project_id_roundtrips() {
    use domain_core::{ProjectId, Timestamp};
    use domain_project::Project;
    use infra_sqlite::SqliteProjectRepository;
    use ports::ProjectRepository;

    let (_dir, db, ws, _rb, _ts) = setup_with_db().await;
    let projects = SqliteProjectRepository::new(db);

    // workspaces.project_id is a FK to projects(id) — the parent row must
    // exist before a workspace can claim it.
    let project_id = ProjectId::parse("PVT_kwHO_bound").unwrap();
    let project = Project::new(
        project_id.clone(),
        "acme".into(),
        1,
        "scratch".into(),
        "PVTSSF_x".into(),
        Vec::new(),
        Vec::new(),
        false,
        Timestamp::now(),
    )
    .unwrap();
    projects.save(&project).await.unwrap();

    let mut workspace = Workspace::new(WorkspaceName::new("project-bound").unwrap(), None, false);
    workspace.project_id = Some(project_id);
    ws.save(&workspace).await.unwrap();

    let back = ws.get(workspace.id).await.unwrap();
    assert_eq!(
        back.project_id.as_ref().map(|p| p.as_str()),
        Some("PVT_kwHO_bound")
    );
}

#[tokio::test]
async fn task_remote_node_id_and_project_item_id_roundtrip() {
    let (_dir, ws, rb, ts) = setup().await;

    let workspace = Workspace::new(WorkspaceName::new("nodes").unwrap(), None, true);
    ws.save(&workspace).await.unwrap();
    let binding = RepoBinding::new(
        workspace.id,
        "git@github.com:o/r.git".into(),
        "github.com/o/r".into(),
    )
    .unwrap();
    rb.save(&binding).await.unwrap();

    let mut task = Task::new_draft(workspace.id, Some(binding.id), "with node ids".into()).unwrap();
    task.stage_for_sync().unwrap();
    let mut remote = RemoteRef::new("github", "42");
    remote.node_id = Some("I_kwHO_xyz".into());
    task.promote_to_remote(remote).unwrap();
    task.project_item_id = Some("PVTI_kwHO_item".into());
    ts.save(&task, SnapshotSource::Promote).await.unwrap();

    let back = ts.get(task.id).await.unwrap();
    assert_eq!(
        back.remote.as_ref().and_then(|r| r.node_id.as_deref()),
        Some("I_kwHO_xyz")
    );
    assert_eq!(back.project_item_id.as_deref(), Some("PVTI_kwHO_item"));
}

// ---------- RFC 0001 Stage 6 (#54): claim_next_eligible + backoff ----------

/// Seed a workspace + a task row (the outbox FK needs a real task) and return
/// the SqliteOutboxRepository plus the task id.
async fn outbox_with_task(
    db: &infra_sqlite::Db,
    ts: &SqliteTaskRepository,
) -> (infra_sqlite::SqliteOutboxRepository, domain_core::TaskId) {
    let workspace = Workspace::new(WorkspaceName::new("obx").unwrap(), None, true);
    SqliteWorkspaceRepository::new(db.clone())
        .save(&workspace)
        .await
        .unwrap();
    let task = Task::new_draft(workspace.id, None, "t".into()).unwrap();
    ts.save(&task, SnapshotSource::Created).await.unwrap();
    (
        infra_sqlite::SqliteOutboxRepository::new(db.clone()),
        task.id,
    )
}

#[tokio::test]
async fn claim_respects_per_task_fifo_and_parallel_across_tasks() {
    use domain_sync::{OutboxEntry, OutboxMutation, OutboxStatus};
    use ports::OutboxRepository;

    let (_dir, db, _ws, _rb, ts) = setup_with_db().await;
    let (outbox, task_a) = outbox_with_task(&db, &ts).await;

    // A second task in its own workspace.
    let ws_b = Workspace::new(WorkspaceName::new("obx-b").unwrap(), None, true);
    SqliteWorkspaceRepository::new(db.clone())
        .save(&ws_b)
        .await
        .unwrap();
    let task_b = Task::new_draft(ws_b.id, None, "tb".into()).unwrap();
    ts.save(&task_b, SnapshotSource::Created).await.unwrap();

    let mk = |task: domain_core::TaskId, body: &str| {
        OutboxEntry::new(
            task,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "1".into(),
                title: None,
                body: Some(body.into()),
                closed: None,
            },
        )
    };

    // Task A: two entries (a1 inserted first, a2 second). Task B: one entry.
    // No `sleep()` to stagger `enqueued_at`: the claim tie-breaks on the
    // implicit `rowid` (insertion order), so FIFO holds even at equal
    // timestamps.
    let a1 = mk(task_a, "a1");
    outbox.enqueue(&a1).await.unwrap();
    let a2 = mk(task_a, "a2");
    outbox.enqueue(&a2).await.unwrap();
    let b1 = mk(task_b.id, "b1");
    outbox.enqueue(&b1).await.unwrap();

    let now = domain_core::Timestamp::now();

    // First claim: a1 (oldest pending overall).
    let c1 = outbox.claim_next_eligible(now).await.unwrap().unwrap();
    assert_eq!(c1.id, a1.id);
    assert_eq!(c1.status, OutboxStatus::Inflight);

    // Second claim: a2 is blocked (task A has an inflight head), so the claim
    // returns b1 — parallel across tasks.
    let c2 = outbox.claim_next_eligible(now).await.unwrap().unwrap();
    assert_eq!(c2.id, b1.id, "task A is busy; B is claimable");

    // Third claim: both A and B are inflight ⇒ nothing eligible.
    assert!(outbox.claim_next_eligible(now).await.unwrap().is_none());

    // Finish a1; now a2 becomes claimable (per-task FIFO preserved order).
    outbox.mark_succeeded(a1.id).await.unwrap();
    let c3 = outbox.claim_next_eligible(now).await.unwrap().unwrap();
    assert_eq!(c3.id, a2.id);
}

#[tokio::test]
async fn claim_tie_breaks_equal_timestamps_by_insertion_order() {
    // `enqueued_at` is second-granular, so two same-task entries enqueued in
    // the same second carry EQUAL timestamps. Per-task FIFO must still claim
    // them in insertion order — guaranteed by the `rowid` tie-breaker in the
    // sibling predicate + ORDER BY. Construct the equal-timestamp case
    // explicitly (no reliance on wall-clock granularity) and assert the order.
    use domain_sync::{OutboxEntry, OutboxMutation, OutboxStatus};
    use ports::OutboxRepository;

    let (_dir, db, _ws, _rb, ts) = setup_with_db().await;
    let (outbox, task) = outbox_with_task(&db, &ts).await;

    let shared = domain_core::Timestamp::now();
    let mk = |body: &str| {
        let mut e = OutboxEntry::new(
            task,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "1".into(),
                title: None,
                body: Some(body.into()),
                closed: None,
            },
        );
        // Force identical enqueued_at on both entries.
        e.enqueued_at = shared;
        e
    };

    // first inserted, then second — both with the SAME enqueued_at.
    let first = mk("first");
    let second = mk("second");
    outbox.enqueue(&first).await.unwrap();
    outbox.enqueue(&second).await.unwrap();

    let now = domain_core::Timestamp::from_utc(shared.into_inner() + chrono::Duration::seconds(1));

    // The head claim must return the first-inserted entry even though both
    // share enqueued_at; the tail is then blocked while the head is inflight.
    let head = outbox.claim_next_eligible(now).await.unwrap().unwrap();
    assert_eq!(head.id, first.id, "equal timestamps ⇒ insertion order wins");
    assert_eq!(head.status, OutboxStatus::Inflight);
    assert!(
        outbox.claim_next_eligible(now).await.unwrap().is_none(),
        "tail blocked behind an equal-timestamped inflight head"
    );

    // After the head resolves, the second is claimable next.
    outbox.mark_succeeded(first.id).await.unwrap();
    let tail = outbox.claim_next_eligible(now).await.unwrap().unwrap();
    assert_eq!(tail.id, second.id);
}

#[tokio::test]
async fn claim_honours_next_attempt_at_eligibility_and_round_trips() {
    use domain_sync::{OutboxEntry, OutboxMutation};
    use ports::OutboxRepository;

    let (_dir, db, _ws, _rb, ts) = setup_with_db().await;
    let (outbox, task) = outbox_with_task(&db, &ts).await;

    let entry = OutboxEntry::new(
        task,
        OutboxMutation::UpdateRemote {
            canonical_repo: "github.com/o/r".into(),
            remote_id: "1".into(),
            title: None,
            body: None,
            closed: None,
        },
    );
    outbox.enqueue(&entry).await.unwrap();

    let now = domain_core::Timestamp::now();

    // Claim it, then reschedule with a future next_attempt_at.
    let claimed = outbox.claim_next_eligible(now).await.unwrap().unwrap();
    let future = domain_core::Timestamp::from_utc(now.into_inner() + chrono::Duration::hours(1));
    outbox
        .record_retry(claimed.id, "boom", future)
        .await
        .unwrap();

    // Not eligible at `now` (next_attempt_at is in the future).
    assert!(
        outbox.claim_next_eligible(now).await.unwrap().is_none(),
        "a rescheduled entry isn't claimable before its backoff window"
    );

    // The bumped attempts + next_attempt_at round-trip via list_pending.
    let pending = outbox.list_pending(task).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].attempts, 1);
    assert!(pending[0].next_attempt_at.is_some());

    // Eligible once we claim with a `now` past the window.
    let later =
        domain_core::Timestamp::from_utc(future.into_inner() + chrono::Duration::seconds(1));
    let reclaimed = outbox.claim_next_eligible(later).await.unwrap();
    assert!(reclaimed.is_some(), "claimable after the backoff window");
}

#[tokio::test]
async fn enqueue_if_absent_dedupes_against_non_terminal_and_failed_siblings() {
    // Atomic dedupe + insert (#54): the startup reconcile's
    // `enqueue_if_absent` must insert only when the task has no
    // `pending`/`inflight`/`failed` sibling. Exercise each guard branch plus
    // the "succeeded sibling does not block" case directly at the SQL level.
    use domain_sync::{OutboxEntry, OutboxMutation};
    use ports::OutboxRepository;

    let (_dir, db, _ws, _rb, ts) = setup_with_db().await;
    let (outbox, task) = outbox_with_task(&db, &ts).await;

    let mk = || {
        OutboxEntry::new(
            task,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "1".into(),
                title: None,
                body: None,
                closed: None,
            },
        )
    };

    // First call: no sibling → inserts.
    assert!(
        outbox.enqueue_if_absent(&mk()).await.unwrap(),
        "first enqueue_if_absent inserts (no sibling)"
    );
    assert_eq!(outbox.list_pending(task).await.unwrap().len(), 1);

    // Second call: a pending sibling exists → no insert.
    assert!(
        !outbox.enqueue_if_absent(&mk()).await.unwrap(),
        "a pending sibling blocks the insert"
    );
    assert_eq!(
        outbox.list_pending(task).await.unwrap().len(),
        1,
        "still exactly one entry"
    );

    // Claim the pending one (→ inflight); an inflight sibling also blocks.
    let claimed = outbox
        .claim_next_eligible(domain_core::Timestamp::now())
        .await
        .unwrap()
        .expect("a pending entry");
    assert!(
        !outbox.enqueue_if_absent(&mk()).await.unwrap(),
        "an inflight sibling blocks the insert"
    );

    // Dead-letter it; a failed sibling still blocks (keeps the dead-letter
    // terminal across restarts).
    outbox.mark_failed(claimed.id, "boom").await.unwrap();
    assert!(
        !outbox.enqueue_if_absent(&mk()).await.unwrap(),
        "a dead-lettered sibling blocks the insert"
    );

    // A second task whose ONLY sibling is succeeded → inserts (succeeded is
    // not a blocker).
    let ws2 = Workspace::new(WorkspaceName::new("obx-succ").unwrap(), None, true);
    SqliteWorkspaceRepository::new(db.clone())
        .save(&ws2)
        .await
        .unwrap();
    let task2 = Task::new_draft(ws2.id, None, "t2".into()).unwrap();
    ts.save(&task2, SnapshotSource::Created).await.unwrap();
    let mk2 = || {
        OutboxEntry::new(
            task2.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "2".into(),
                title: None,
                body: None,
                closed: None,
            },
        )
    };
    outbox.enqueue(&mk2()).await.unwrap();
    let claimed2 = outbox
        .claim_next_eligible(domain_core::Timestamp::now())
        .await
        .unwrap()
        .expect("task2 pending entry");
    outbox.mark_succeeded(claimed2.id).await.unwrap();
    assert!(
        outbox.enqueue_if_absent(&mk2()).await.unwrap(),
        "a succeeded sibling does not block a fresh insert"
    );
}
