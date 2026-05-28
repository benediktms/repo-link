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
    RemoteComment, RepoBindingRepository, TaskFilter, TaskRepository, WorkspaceRepository,
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
    // (default_for IS NULL) exactly as it was saved.
    assert_eq!(loaded.option_id_for(TaskStatus::Open), Some("o1"));
    assert_eq!(loaded.option_id_for(TaskStatus::Done), Some("o2"));
    assert_eq!(loaded.option_id_for(TaskStatus::InProgress), None);
}

#[tokio::test]
async fn outbox_next_pending_claims_oldest_and_flips_to_inflight() {
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

    // Enqueue two entries. The drainer should claim the older one first.
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
    // Give e2 a strictly later enqueued_at by sleeping briefly — sub-µs
    // ordering would otherwise be ambiguous on hot machines (we hit this
    // exact thing on the rollback PR).
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
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
    let claimed = outbox.next_pending().await.unwrap().expect("a pending entry");
    assert_eq!(claimed.id, e1.id);
    assert_eq!(claimed.status, OutboxStatus::Inflight);

    // list_pending now sees only e2.
    let pending = outbox.list_pending(task.id).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, e2.id);

    // Mark e1 succeeded; mark_failed e2 to exercise both paths.
    outbox.mark_succeeded(e1.id).await.unwrap();
    outbox
        .mark_failed(e2.id, "graphql 5xx")
        .await
        .unwrap();
    // After mark_failed, e2 is no longer in `pending`.
    let pending = outbox.list_pending(task.id).await.unwrap();
    assert!(pending.is_empty());
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

    let mut workspace = Workspace::new(
        WorkspaceName::new("project-bound").unwrap(),
        None,
        false,
    );
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

    let mut task = Task::new_draft(
        workspace.id,
        Some(binding.id),
        "with node ids".into(),
    )
    .unwrap();
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
