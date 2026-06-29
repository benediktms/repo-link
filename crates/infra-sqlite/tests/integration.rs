use std::path::PathBuf;
use std::sync::Arc;

use domain_core::{RepoId, RepoOriginId};
use domain_repo::{RepoInstance, RepoOrigin};
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

/// RFC 0005 test helper: persist a shared `RepoOrigin` + a per-workspace
/// `RepoInstance` for it, returning the instance (whose `id` is what a task's
/// logical `repo_id` points at). An optional `prefix` overrides the
/// auto-derived one so callers can dodge the `repo_origins.prefix` UNIQUE index
/// when seeding several look-alike canonicals. The instance carries
/// `instance.origin_id`, which is the ORIGIN id space `filing_repo_id` /
/// `find_by_remote` / `find_by_remote_mapping` operate in.
async fn seed_binding(
    rb: &SqliteRepoBindingRepository,
    workspace_id: domain_core::WorkspaceId,
    remote_url: &str,
    canonical_url: &str,
    prefix: Option<&str>,
) -> RepoInstance {
    let mut origin = RepoOrigin::new(remote_url.into(), canonical_url.into()).unwrap();
    if let Some(p) = prefix {
        origin.set_prefix(p.into()).unwrap();
    }
    rb.save_origin(&origin).await.unwrap();
    let instance = RepoInstance::new(workspace_id, origin.id, canonical_url.into(), None).unwrap();
    rb.save_instance(&instance).await.unwrap();
    instance
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

    // Two bindings in the same workspace, each with its own shared origin.
    let repo_a = seed_binding(
        &rb,
        w.id,
        "git@github.com:o/a.git",
        "github.com/o/a",
        Some("aaa"),
    )
    .await;
    let repo_b = seed_binding(
        &rb,
        w.id,
        "git@github.com:o/b.git",
        "github.com/o/b",
        Some("bbb"),
    )
    .await;

    // A task in `repo` mirroring github issue `num`. RFC 0005: the remote
    // axis is keyed on the FILING repo in ORIGIN id space, so record the
    // logical repo's origin as the filing repo (mirrors the migration's 7a
    // straggler backfill) before promoting.
    let mk = |instance: &RepoInstance, num: &str| {
        let mut t = Task::new_draft(w.id, Some(instance.id), format!("issue {num}")).unwrap();
        t.set_filing_repo_id(Some(RepoId::from_uuid(instance.origin_id.as_uuid())))
            .unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", num)).unwrap();
        t
    };

    // Same issue number (#1) in two different repos must both persist —
    // remote identity is repo-scoped, so they don't collide.
    ts.save(&mk(&repo_a, "1"), SnapshotSource::Promote)
        .await
        .unwrap();
    ts.save(&mk(&repo_b, "1"), SnapshotSource::Promote)
        .await
        .expect("repoB#1 must not collide with repoA#1");

    // But the same (repo, provider, remote_id) still conflicts.
    let err = ts
        .save(&mk(&repo_a, "1"), SnapshotSource::Promote)
        .await
        .expect_err("duplicate remote in the same repo should conflict");
    let msg = format!("{err:?}").to_lowercase();
    assert!(
        msg.contains("unique") || msg.contains("conflict"),
        "got: {err:?}"
    );
}

/// RFC 0002 D6: remote dedup is keyed on the FILING repo, not the logical repo.
/// A task whose filing repo diverges from its logical repo is found by its
/// filing repo (read side, `find_by_remote`) and collides on
/// (filing_repo_id, provider, remote_id) (write side, the UNIQUE key) — and is
/// NOT found by its logical repo.
#[tokio::test]
async fn remote_dedup_keyed_on_filing_repo_when_it_diverges() {
    let (_dir, ws, rb, ts) = setup().await;
    let w = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
    ws.save(&w).await.unwrap();

    let logical = seed_binding(
        &rb,
        w.id,
        "git@github.com:o/logical.git",
        "github.com/o/logical",
        None,
    )
    .await;
    let filing = seed_binding(
        &rb,
        w.id,
        "git@github.com:o/filing.git",
        "github.com/o/filing",
        None,
    )
    .await;
    // RFC 0005: the filing axis is in ORIGIN id space.
    let filing_origin = RepoOriginId::from_uuid(filing.origin_id.as_uuid());
    let logical_origin = RepoOriginId::from_uuid(logical.origin_id.as_uuid());

    // Logical repo = `logical`, but the issue is FILED in `filing`.
    let mk = |num: &str| {
        let mut t = Task::new_draft(w.id, Some(logical.id), format!("issue {num}")).unwrap();
        t.set_filing_repo_id(Some(RepoId::from_uuid(filing.origin_id.as_uuid())))
            .unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", num)).unwrap();
        t
    };

    ts.save(&mk("1"), SnapshotSource::Promote).await.unwrap();

    // Read side: found by the FILING repo, not the logical repo.
    assert!(
        ts.find_by_remote(filing_origin, "github", "1")
            .await
            .unwrap()
            .is_some(),
        "dedup must match on the filing repo"
    );
    assert!(
        ts.find_by_remote(logical_origin, "github", "1")
            .await
            .unwrap()
            .is_none(),
        "dedup must NOT match on the logical repo once filing diverges (D6)"
    );

    // Write side: a second task filed in the same repo with the same remote
    // collides on (filing_repo_id, provider, remote_id).
    let err = ts
        .save(&mk("1"), SnapshotSource::Promote)
        .await
        .expect_err("duplicate filing-scoped remote must conflict");
    let msg = format!("{err:?}").to_lowercase();
    assert!(
        msg.contains("unique") || msg.contains("conflict"),
        "got: {err:?}"
    );
}

/// RFC 0005 §D4: the COALESCE-to-logical fallback is GONE — `find_by_remote`
/// keys on `filing_repo_id` alone (ORIGIN id space). A task whose logical repo
/// is its filing repo records the logical repo's *origin* as the filing repo
/// (the runtime equivalent of the migration's 7a straggler backfill), and is
/// then found by that ORIGIN id — not by the logical *instance* id.
#[tokio::test]
async fn remote_dedup_keys_on_filing_origin_not_logical_instance() {
    let (_dir, ws, rb, ts) = setup().await;
    let w = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
    ws.save(&w).await.unwrap();
    let repo = seed_binding(&rb, w.id, "git@github.com:o/r.git", "github.com/o/r", None).await;
    let repo_origin = RepoOriginId::from_uuid(repo.origin_id.as_uuid());

    // Logical repo IS the filing repo: record its origin as the filing repo.
    let mut t = Task::new_draft(w.id, Some(repo.id), "issue 7".into()).unwrap();
    t.set_filing_repo_id(Some(RepoId::from_uuid(repo.origin_id.as_uuid())))
        .unwrap();
    t.stage_for_sync().unwrap();
    t.promote_to_remote(RemoteRef::new("github", "7")).unwrap();
    ts.save(&t, SnapshotSource::Promote).await.unwrap();

    // Found by the filing repo's ORIGIN id.
    assert!(
        ts.find_by_remote(repo_origin, "github", "7")
            .await
            .unwrap()
            .is_some(),
        "remote-backed task is found by its recorded filing-repo origin id"
    );
    // NOT found by the logical *instance* id (a different id space).
    assert!(
        ts.find_by_remote(RepoOriginId::from_uuid(repo.id.as_uuid()), "github", "7")
            .await
            .unwrap()
            .is_none(),
        "find_by_remote keys on filing_repo_id alone — the logical instance id no longer matches"
    );
}

#[tokio::test]
async fn repo_with_worktrees_roundtrip() {
    let (_dir, ws, rb, _ts) = setup().await;
    let w = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
    ws.save(&w).await.unwrap();
    let origin = RepoOrigin::new("git@github.com:o/r.git".into(), "github.com/o/r".into()).unwrap();
    rb.save_origin(&origin).await.unwrap();
    let mut binding = RepoInstance::new(w.id, origin.id, "github.com/o/r".into(), None).unwrap();
    binding.link_worktree(PathBuf::from("/tmp/a"), Some("main".into()));
    binding.link_worktree(PathBuf::from("/tmp/b"), None);
    binding
        .mark_path_missing(std::path::Path::new("/tmp/b"))
        .unwrap();
    rb.save_instance(&binding).await.unwrap();

    let back = rb.get(binding.id).await.unwrap().instance;
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
    rb.save_instance(&updated).await.unwrap();
    let after = rb.get(binding.id).await.unwrap().instance;
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

    // Open-only list excludes the closed (archived) task (RFC 0004 D1: no
    // implicit archived-hiding anymore — filter on the open/closed bit).
    let live = ts
        .list(TaskFilter {
            is_open: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(live.len(), 1);
    let all = ts.list(TaskFilter::default()).await.unwrap();
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

/// The poller's per-tick stale-scan (RFC 0004 D3): `has_project_item_id` +
/// `pollable_workspaces_only` + `synced_at_lt` + `limit`, ordered
/// oldest-`synced_at`-first (NULLs first). Also the pollable-gate tripwire — the
/// gate is `status = 'active' AND project_id IS NOT NULL`, so a non-active
/// workspace's tasks AND a projectless workspace's tasks are both excluded (a
/// future `Deleted`/`Paused` variant stays excluded by the explicit equality).
#[tokio::test]
async fn list_poll_scan_gates_and_orders() {
    use domain_core::{ProjectId, Timestamp};
    use domain_project::Project;
    use infra_sqlite::SqliteProjectRepository;
    use ports::ProjectRepository;

    let (_dir, db, ws, _rb, ts) = setup_with_db().await;
    let projects = SqliteProjectRepository::new(db.clone());

    // Two real projects so the `workspaces.project_id` FK is satisfied.
    let mk_project = |node: &str| {
        Project::new(
            ProjectId::parse(node).unwrap(),
            "acme".into(),
            1,
            "Board".into(),
            "PVTSSF_f".into(),
            vec![],
            vec![],
            false,
            Timestamp::now(),
        )
        .unwrap()
    };
    let pa = mk_project("PVT_kwHO_active");
    let pc = mk_project("PVT_kwHO_created");
    projects.save(&pa).await.unwrap();
    projects.save(&pc).await.unwrap();

    // Active + project-attached → pollable.
    let mut active = Workspace::new(WorkspaceName::new("active").unwrap(), None, true);
    active.activate().unwrap();
    active.project_id = Some(pa.id.clone());
    // Active but projectless → NOT pollable (a stale project_item_id here could
    // otherwise loop forever).
    let mut projectless = Workspace::new(WorkspaceName::new("projectless").unwrap(), None, true);
    projectless.activate().unwrap();
    // Not active → NOT pollable (even though project-attached).
    let mut created = Workspace::new(WorkspaceName::new("created").unwrap(), None, true);
    created.project_id = Some(pc.id.clone());
    ws.save(&active).await.unwrap();
    ws.save(&projectless).await.unwrap();
    ws.save(&created).await.unwrap();

    // Helper: a project-backed task with an explicit synced_at, saved once
    // (INSERT persists synced_at).
    let mk = |wsid, title: &str, item: Option<&str>, synced: Option<Timestamp>| {
        let mut t = Task::new_draft(wsid, None, title.into()).unwrap();
        t.project_item_id = item.map(str::to_string);
        t.synced_at = synced;
        t
    };
    let now = Timestamp::now();
    let old = Timestamp::from_utc(now.into_inner() - chrono::Duration::hours(1));

    let never = mk(active.id, "never-observed", Some("PVTI_never"), None);
    let stale = mk(active.id, "stale", Some("PVTI_stale"), Some(old));
    let no_item = mk(active.id, "no-item", None, None);
    let inactive = mk(created.id, "inactive-ws", Some("PVTI_inact"), None);
    let orphan = mk(projectless.id, "projectless-ws", Some("PVTI_orph"), None);
    for t in [&never, &stale, &no_item, &inactive, &orphan] {
        ts.save(t, SnapshotSource::LocalEdit).await.unwrap();
    }

    let scan = ts
        .list(TaskFilter {
            has_project_item_id: true,
            pollable_workspaces_only: true,
            synced_at_lt: Some(now),
            limit: Some(10),
            ..Default::default()
        })
        .await
        .unwrap();
    // Only the two pollable (active + project-attached) + project-backed + stale
    // tasks; ordered NULLs first. `no-item` (no item), `inactive-ws` (not
    // active), and `projectless-ws` (no project) are all excluded.
    assert_eq!(
        scan.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
        vec!["never-observed", "stale"],
        "pollable+project-backed+stale only, oldest-synced first"
    );

    // The LIMIT caps the scan, keeping the stalest (NULL synced_at) first.
    let capped = ts
        .list(TaskFilter {
            has_project_item_id: true,
            pollable_workspaces_only: true,
            synced_at_lt: Some(now),
            limit: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(capped.len(), 1);
    assert_eq!(capped[0].title, "never-observed");
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
    t.complete().unwrap();

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
    assert_eq!(back.lifecycle, domain_task::Lifecycle::Completed);
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
async fn save_many_with_outbox_persists_both_sides_and_entry_in_one_tx() {
    use domain_sync::{OutboxEntry, OutboxMutation};
    use infra_sqlite::SqliteOutboxRepository;
    use ports::OutboxRepository;

    let (_dir, db, ws, _rb, ts) = setup_with_db().await;
    let w = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
    ws.save(&w).await.unwrap();

    // Both ends already exist (relation ops only batch already-loaded tasks).
    let mut a = Task::new_draft(w.id, None, "A".into()).unwrap();
    let mut b = Task::new_draft(w.id, None, "B".into()).unwrap();
    ts.save(&a, SnapshotSource::Created).await.unwrap();
    ts.save(&b, SnapshotSource::Created).await.unwrap();

    // The reciprocal halves of a blocked_by/blocks pair PLUS the single
    // dependency mutation the edit owes — all in one atomic write.
    a.add_relation(RelationKind::BlockedBy, b.id);
    b.add_relation(RelationKind::Blocks, a.id);
    let entry = OutboxEntry::new(
        a.id,
        OutboxMutation::AddBlockedBy {
            blocked_canonical: "github.com/o/r".into(),
            blocked_remote_id: "10".into(),
            blocker_canonical: "github.com/o/r".into(),
            blocker_remote_id: "20".into(),
        },
    );
    ts.save_many_with_outbox(
        &[
            (&a, SnapshotSource::LocalEdit),
            (&b, SnapshotSource::LocalEdit),
        ],
        std::slice::from_ref(&entry),
    )
    .await
    .unwrap();

    // Both reciprocal edges round-trip.
    assert_eq!(ts.get(a.id).await.unwrap().relations.len(), 1);
    assert_eq!(ts.get(b.id).await.unwrap().relations.len(), 1);
    // The dependency entry is pending, keyed on the command subject `a`.
    let outbox = SqliteOutboxRepository::new(db);
    let pending = outbox.list_pending(a.id).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].mutation.kind(), "add_blocked_by");
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
    let (_dir, db, _ws, _rb, _ts) = setup_with_db().await;

    // RFC 0005: name/aliases now live on `repo_origins`. Insert an origin row
    // with name = '' directly, bypassing the repository layer to simulate a
    // row that predates the name column.
    sqlx::query(
        "INSERT INTO repo_origins (id, canonical_url, remote_url, prefix, name, aliases, created_at, updated_at) \
         VALUES (?, ?, ?, '', '', '[]', datetime('now'), datetime('now'))",
    )
    .bind("aaaaaaaa-0000-0000-0000-000000000001")
    .bind("github.com/org/myrepo")
    .bind("git@github.com:org/myrepo.git")
    .execute(&db.writes)
    .await
    .unwrap();

    // Run the backfill — should derive "myrepo" from the canonical URL.
    backfill_empty_repo_names(&db.writes).await.unwrap();

    let (name,): (String,) = sqlx::query_as("SELECT name FROM repo_origins WHERE id = ?")
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

    let binding = seed_binding(
        &rb,
        w.id,
        "git@github.com:org/proj.git",
        "github.com/org/proj",
        None,
    )
    .await;

    // Running backfill again should be a no-op and not error.
    backfill_empty_repo_names(&db.writes).await.unwrap();

    let (name,): (String,) = sqlx::query_as("SELECT name FROM repo_origins WHERE id = ?")
        .bind(binding.origin_id.to_string())
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

/// The CHECK constraint on `repo_origins.aliases` must reject valid JSON that
/// isn't a JSON array. Without `json_type(...) = 'array'`, an object or
/// scalar would slip through and break Vec<String> hydration on load.
/// (RFC 0005: name/aliases moved from `repos` to the shared `repo_origins`.)
#[tokio::test]
async fn aliases_check_rejects_non_array_json() {
    let (_dir, db, _ws, _rb, _ts) = setup_with_db().await;

    // Try to insert an origin row whose aliases is a valid JSON *object*
    // (not an array). The CHECK constraint must reject this.
    let result = sqlx::query(
        r#"
        INSERT INTO repo_origins (id, canonical_url, remote_url, prefix,
                                  name, aliases, created_at, updated_at)
        VALUES (?, ?, ?, '', ?, ?, ?, ?)
        "#,
    )
    .bind("c08c09c5-4ac2-4a43-96ea-d574a580fde5")
    .bind("example.com/o/r")
    .bind("git@example.com:o/r.git")
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
                is_open: true,
                option_id: "o1".into(),
            },
            StatusMapping {
                is_open: false,
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
    assert_eq!(loaded.option_id_for(true), Some("o1"));
    assert_eq!(loaded.option_id_for(false), Some("o2"));
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
                is_open: true,
                option_id: "backlog".into(),
            },
            StatusMapping {
                is_open: false,
                option_id: "done".into(),
            },
        ],
        false,
        Timestamp::now(),
    )
    .unwrap();

    projects.save(&saved).await.unwrap();
    let loaded = projects.get(id).await.unwrap();

    // Both open/closed mappings round-trip (RFC 0004 D1: the lifecycle
    // collapsed to the open/closed bit, so there are at most two rows).
    assert_eq!(loaded.status_mappings.len(), 2);
    assert_eq!(loaded.option_id_for(true), Some("backlog"));
    assert_eq!(loaded.option_id_for(false), Some("done"));
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

/// RFC 0002 (#116): the workspace default `filing_repo_id` round-trips through
/// `save` (insert) and the upsert DO-UPDATE half. NULL until set, then a repo
/// id sticks.
#[tokio::test]
async fn workspace_filing_repo_id_roundtrips() {
    let (_dir, ws, rb, _ts) = setup().await;

    let mut workspace = Workspace::new(WorkspaceName::new("filing-ws").unwrap(), None, true);
    ws.save(&workspace).await.unwrap();
    let binding = seed_binding(
        &rb,
        workspace.id,
        "git@github.com:o/r.git",
        "github.com/o/r",
        None,
    )
    .await;
    // RFC 0005: the workspace default filing repo is in ORIGIN id space.
    let filing_default = RepoId::from_uuid(binding.origin_id.as_uuid());

    // Insert path: no default filing repo yet.
    assert_eq!(
        ws.get(workspace.id).await.unwrap().filing_repo_id,
        None,
        "no workspace default filing repo until one is set"
    );

    // Upsert path: set the default and re-save; the DO UPDATE must persist it.
    workspace.filing_repo_id = Some(filing_default);
    ws.save(&workspace).await.unwrap();
    assert_eq!(
        ws.get(workspace.id).await.unwrap().filing_repo_id,
        Some(filing_default),
        "workspace default filing repo round-trips through the upsert"
    );
}

#[tokio::test]
async fn task_remote_node_id_and_project_item_id_roundtrip() {
    let (_dir, ws, rb, ts) = setup().await;

    let workspace = Workspace::new(WorkspaceName::new("nodes").unwrap(), None, true);
    ws.save(&workspace).await.unwrap();
    let binding = seed_binding(
        &rb,
        workspace.id,
        "git@github.com:o/r.git",
        "github.com/o/r",
        None,
    )
    .await;

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

/// RFC 0001 Stage 8 (#56, closes #39): the cached `project_status_option_id`
/// round-trips through `write_task_in_tx` + `row_to_task` on the FULL upsert
/// lifecycle — insert (None), upsert-update to Some, reload, then upsert-update
/// changing it again, reload. The DO-UPDATE half is the bug class: forgetting it
/// silently never persists the change on the (existing-row) upsert path the
/// poller always hits.
#[tokio::test]
async fn task_project_status_option_id_roundtrips_through_upsert() {
    let (_dir, ws, _rb, ts) = setup().await;

    let workspace = Workspace::new(WorkspaceName::new("pstatus").unwrap(), None, true);
    ws.save(&workspace).await.unwrap();

    // Insert: a fresh task carries no cached project status.
    let mut task = Task::new_draft(workspace.id, None, "cached status".into()).unwrap();
    task.stage_for_sync().unwrap();
    task.promote_to_remote(RemoteRef::new("github", "1"))
        .unwrap();
    ts.save(&task, SnapshotSource::Promote).await.unwrap();
    assert_eq!(
        ts.get(task.id).await.unwrap().project_status_option_id,
        None,
        "fresh task has no cached project status"
    );

    // Upsert-update: the poller writes the polled option id. This hits the
    // ON CONFLICT DO UPDATE branch (the row already exists).
    assert!(task.set_project_status_option_id(Some("o_wip".into())));
    ts.save(&task, SnapshotSource::LocalEdit).await.unwrap();
    assert_eq!(
        ts.get(task.id)
            .await
            .unwrap()
            .project_status_option_id
            .as_deref(),
        Some("o_wip"),
        "DO UPDATE must persist the cached status on upsert"
    );

    // Upsert-update again: a later poll moved the card to a different option.
    assert!(task.set_project_status_option_id(Some("o_done".into())));
    ts.save(&task, SnapshotSource::LocalEdit).await.unwrap();
    assert_eq!(
        ts.get(task.id)
            .await
            .unwrap()
            .project_status_option_id
            .as_deref(),
        Some("o_done"),
        "a subsequent upsert overwrites the cached status"
    );
}

/// RFC 0002 (#116): `filing_repo_id` round-trips through `write_task_in_tx` +
/// `row_to_task` across the full upsert lifecycle — insert (None), then the
/// promote-time recording write (None → Some, allowed even though the task is
/// already remote-backed) through the DO-UPDATE half, then reload.
#[tokio::test]
async fn task_filing_repo_id_roundtrips_through_upsert() {
    let (_dir, ws, rb, ts) = setup().await;

    let workspace = Workspace::new(WorkspaceName::new("filing-task").unwrap(), None, true);
    ws.save(&workspace).await.unwrap();
    let binding = seed_binding(
        &rb,
        workspace.id,
        "git@github.com:o/r.git",
        "github.com/o/r",
        None,
    )
    .await;
    // RFC 0005: filing_repo_id is in ORIGIN id space.
    let filing = RepoId::from_uuid(binding.origin_id.as_uuid());

    // Insert: a fresh task has no resolved filing repo.
    let mut task = Task::new_draft(workspace.id, Some(binding.id), "filing".into()).unwrap();
    task.stage_for_sync().unwrap();
    task.promote_to_remote(RemoteRef::new("github", "1"))
        .unwrap();
    ts.save(&task, SnapshotSource::Promote).await.unwrap();
    assert_eq!(
        ts.get(task.id).await.unwrap().filing_repo_id,
        None,
        "fresh task has no resolved filing repo"
    );

    // Record the filing repo (the promote-time write: None → Some is allowed
    // even once remote-backed) and upsert; the DO UPDATE must persist it.
    task.set_filing_repo_id(Some(filing)).unwrap();
    ts.save(&task, SnapshotSource::LocalEdit).await.unwrap();
    assert_eq!(
        ts.get(task.id).await.unwrap().filing_repo_id,
        Some(filing),
        "DO UPDATE must persist the recorded filing repo on upsert"
    );
}

/// RFC 0002 #118: the resolved filing repo round-trips into snapshot history
/// and reads back via `SqliteTaskSnapshotRepository::get` / `list`.
#[tokio::test]
async fn snapshot_filing_repo_id_roundtrips() {
    use infra_sqlite::SqliteTaskSnapshotRepository;
    use ports::TaskSnapshotRepository;

    let (_dir, db, ws, rb, ts) = setup_with_db().await;
    let workspace = Workspace::new(WorkspaceName::new("snap-filing").unwrap(), None, true);
    ws.save(&workspace).await.unwrap();
    let binding = seed_binding(
        &rb,
        workspace.id,
        "git@github.com:o/r.git",
        "github.com/o/r",
        None,
    )
    .await;
    // RFC 0005: filing_repo_id is in ORIGIN id space.
    let filing = RepoId::from_uuid(binding.origin_id.as_uuid());

    // Promote then record the filing repo, then write a Promote snapshot
    // carrying it.
    let mut task = Task::new_draft(workspace.id, Some(binding.id), "snap".into()).unwrap();
    task.stage_for_sync().unwrap();
    task.promote_to_remote(RemoteRef::new("github", "7"))
        .unwrap();
    task.set_filing_repo_id(Some(filing)).unwrap();
    ts.save(&task, SnapshotSource::Promote).await.unwrap();

    let snaps = SqliteTaskSnapshotRepository::new(db);
    let listed = snaps.list(task.id).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].filing_repo_id,
        Some(filing),
        "list must read back the captured filing repo"
    );
    let got = snaps.get(task.id, listed[0].version).await.unwrap();
    assert_eq!(
        got.filing_repo_id,
        Some(filing),
        "get must read back the captured filing repo"
    );
}

/// RFC 0002 #118: the second read path — baseline hydration via
/// `load_latest_baseline` (exercised by `SqliteTaskRepository::get`) — must
/// also carry the filing repo.
#[tokio::test]
async fn load_latest_baseline_carries_filing_repo_id() {
    let (_dir, ws, rb, ts) = setup().await;
    let workspace = Workspace::new(WorkspaceName::new("snap-baseline").unwrap(), None, true);
    ws.save(&workspace).await.unwrap();
    let binding = seed_binding(
        &rb,
        workspace.id,
        "git@github.com:o/r.git",
        "github.com/o/r",
        None,
    )
    .await;
    // RFC 0005: filing_repo_id is in ORIGIN id space.
    let filing = RepoId::from_uuid(binding.origin_id.as_uuid());

    let mut task = Task::new_draft(workspace.id, Some(binding.id), "baseline".into()).unwrap();
    task.stage_for_sync().unwrap();
    task.promote_to_remote(RemoteRef::new("github", "8"))
        .unwrap();
    task.set_filing_repo_id(Some(filing)).unwrap();
    // Promote IS baseline-eligible, so this snapshot becomes the baseline.
    ts.save(&task, SnapshotSource::Promote).await.unwrap();

    let reloaded = ts.get(task.id).await.unwrap();
    assert_eq!(
        reloaded.synced_baseline.unwrap().filing_repo_id,
        Some(filing),
        "the hydrated baseline must carry the filing repo"
    );
}

/// RFC 0002 #118: snapshot rows pre-dating the `filing_repo_id` column (or with
/// an empty value) must read back as `None` gracefully — the same tolerance the
/// `repo_id` non-empty/parse path established. We simulate a pre-column row by
/// inserting one with a NULL `filing_repo_id` directly.
#[tokio::test]
async fn pre_column_snapshot_filing_reads_null() {
    use infra_sqlite::SqliteTaskSnapshotRepository;
    use ports::TaskSnapshotRepository;

    let (_dir, db, ws, _rb, ts) = setup_with_db().await;
    let workspace = Workspace::new(WorkspaceName::new("snap-precolumn").unwrap(), None, true);
    ws.save(&workspace).await.unwrap();

    // A real task to satisfy the FK from task_snapshots.task_id → tasks.id.
    let task = Task::new_draft(workspace.id, None, "precolumn".into()).unwrap();
    ts.save(&task, SnapshotSource::Created).await.unwrap();

    // Insert a raw snapshot row with NULL filing_repo_id, as a pre-column row
    // would look after the additive migration backfilled it NULL.
    sqlx::query(
        "INSERT INTO task_snapshots \
         (task_id, version, title, body, status, sync_state, priority, assignees_json, \
          remote_provider, remote_id, repo_id, repo_id_recorded, filing_repo_id, source, captured_at) \
         VALUES (?, 99, 'old', '', 'open', 'local_only', 'p3', '[]', NULL, NULL, NULL, 0, NULL, 'local_edit', ?)",
    )
    .bind(task.id.to_string())
    .bind(domain_core::Timestamp::now().into_inner())
    .execute(&db.writes)
    .await
    .unwrap();

    let snaps = SqliteTaskSnapshotRepository::new(db);
    let got = snaps.get(task.id, 99).await.unwrap();
    assert_eq!(
        got.filing_repo_id, None,
        "a NULL/pre-column filing_repo_id must read back as None"
    );
}

/// The same column must persist through `save_with_outbox`, which shares the
/// `write_task_in_tx` write path with `save`.
#[tokio::test]
async fn save_with_outbox_persists_project_status_option_id() {
    use domain_sync::{OutboxEntry, OutboxMutation};

    let (_dir, ws, _rb, ts) = setup().await;
    let workspace = Workspace::new(WorkspaceName::new("pstatus-obx").unwrap(), None, true);
    ws.save(&workspace).await.unwrap();

    let mut task = Task::new_draft(workspace.id, None, "via outbox".into()).unwrap();
    task.stage_for_sync().unwrap();
    task.promote_to_remote(RemoteRef::new("github", "1"))
        .unwrap();
    task.set_project_status_option_id(Some("o_review".into()));

    let entry = OutboxEntry::new(
        task.id,
        OutboxMutation::UpdateRemote {
            canonical_repo: "github.com/o/r".into(),
            remote_id: "1".into(),
            title: None,
            body: Some("b".into()),
            closed: None,
        },
    );
    ts.save_with_outbox(&task, SnapshotSource::Promote, &[entry])
        .await
        .unwrap();

    assert_eq!(
        ts.get(task.id)
            .await
            .unwrap()
            .project_status_option_id
            .as_deref(),
        Some("o_review"),
        "save_with_outbox shares write_task_in_tx, so the cache column persists too"
    );
}

/// RFC 0001 Stage 8 (#56, closes #39, thread r3325841752): `cache_project_status`
/// is a targeted single-column write — it updates ONLY
/// `project_status_option_id` and leaves title / body / status / sync_state
/// untouched. `None` clears the column; an absent id is a benign no-op Ok.
#[tokio::test]
async fn cache_project_status_writes_only_the_cache_column() {
    let (_dir, ws, _rb, ts) = setup().await;

    let workspace = Workspace::new(WorkspaceName::new("cache-only").unwrap(), None, true);
    ws.save(&workspace).await.unwrap();

    let mut task = Task::new_draft(workspace.id, None, "seed title".into()).unwrap();
    task.set_body("seed body".into());
    task.stage_for_sync().unwrap();
    task.promote_to_remote(RemoteRef::new("github", "1"))
        .unwrap();
    ts.save(&task, SnapshotSource::Promote).await.unwrap();

    let before = ts.get(task.id).await.unwrap();
    assert_eq!(before.project_status_option_id, None);

    // Targeted write: set the cache option id.
    ts.cache_project_status(task.id, Some("o_wip".into()))
        .await
        .unwrap();

    let after = ts.get(task.id).await.unwrap();
    assert_eq!(
        after.project_status_option_id.as_deref(),
        Some("o_wip"),
        "the cache column is updated"
    );
    // Every other column is byte-for-byte the pre-call value.
    assert_eq!(after.title, before.title, "title unchanged");
    assert_eq!(after.body, before.body, "body unchanged");
    assert_eq!(after.lifecycle, before.lifecycle, "lifecycle unchanged");
    assert_eq!(after.sync, before.sync, "sync_state unchanged");

    // None clears the column.
    ts.cache_project_status(task.id, None).await.unwrap();
    assert_eq!(
        ts.get(task.id).await.unwrap().project_status_option_id,
        None,
        "binding None clears the cache column"
    );

    // An absent id is a benign no-op Ok (zero rows matched).
    ts.cache_project_status(domain_core::TaskId::new(), Some("o_ghost".into()))
        .await
        .expect("cache_project_status for an absent task is a no-op Ok");
}

/// RFC 0001 Stage 8 (#56, thread r3325841752) — the regression the fix exists
/// for: a `cache_project_status` write must NOT clobber a concurrent whole-row
/// edit. Persist a task; load a stale copy; via the repo, save a NEW title
/// (simulating a concurrent CLI edit); THEN cache the project status for the
/// same id. The NEW title must survive AND the cache must be set. (With the old
/// full-row `save` from the stale copy, the title would regress.)
#[tokio::test]
async fn cache_project_status_does_not_clobber_concurrent_whole_row_edit() {
    let (_dir, ws, _rb, ts) = setup().await;

    let workspace = Workspace::new(WorkspaceName::new("no-clobber").unwrap(), None, true);
    ws.save(&workspace).await.unwrap();

    let mut task = Task::new_draft(workspace.id, None, "original title".into()).unwrap();
    task.stage_for_sync().unwrap();
    task.promote_to_remote(RemoteRef::new("github", "1"))
        .unwrap();
    ts.save(&task, SnapshotSource::Promote).await.unwrap();

    // A stale snapshot — the shape the poller holds in its per-pass index.
    let _stale = ts.get(task.id).await.unwrap();

    // A concurrent CLI edit lands a NEW title via the whole-row save path.
    let mut concurrent = ts.get(task.id).await.unwrap();
    concurrent
        .set_title("edited by concurrent CLI".into())
        .unwrap();
    ts.save(&concurrent, SnapshotSource::LocalEdit)
        .await
        .unwrap();

    // The poller now caches the polled status for the SAME id. The targeted
    // single-column write must leave the freshly-edited title intact.
    ts.cache_project_status(task.id, Some("o_done".into()))
        .await
        .unwrap();

    let reloaded = ts.get(task.id).await.unwrap();
    assert_eq!(
        reloaded.title, "edited by concurrent CLI",
        "the concurrent title edit must survive the cache write (no whole-row clobber)"
    );
    assert_eq!(
        reloaded.project_status_option_id.as_deref(),
        Some("o_done"),
        "the cache column is set by the targeted write"
    );
}

/// rpl-4ui (RFC 0001 §9 / §D1) — `cache_remote_node_id` backfills ONLY the
/// `remote_node_id` column for a pre-project-sync task, and survives a
/// concurrent whole-row edit just like `cache_project_status`. Promote with a
/// bare `RemoteRef::new` (node id `None`, the pre-Stage-2 shape), confirm it
/// round-trips as `None`, then backfill and assert it sticks without tearing a
/// concurrent title edit.
#[tokio::test]
async fn cache_remote_node_id_backfills_without_clobbering_concurrent_edit() {
    let (_dir, ws, _rb, ts) = setup().await;

    let workspace = Workspace::new(WorkspaceName::new("node-id-backfill").unwrap(), None, true);
    ws.save(&workspace).await.unwrap();

    let mut task = Task::new_draft(workspace.id, None, "original title".into()).unwrap();
    task.stage_for_sync().unwrap();
    // Bare ref → node_id None: the row a task created before node ids were
    // persisted looks like.
    task.promote_to_remote(RemoteRef::new("github", "100"))
        .unwrap();
    ts.save(&task, SnapshotSource::Promote).await.unwrap();

    let before = ts.get(task.id).await.unwrap();
    assert_eq!(
        before.remote.as_ref().unwrap().node_id,
        None,
        "pre-backfill the node id round-trips as None"
    );

    // A concurrent CLI edit lands a NEW title via the whole-row save path.
    // `set_title` flips the task Synced -> DirtyLocal via dirty detection; the
    // backfill below must preserve THAT state, not reset it.
    let mut concurrent = ts.get(task.id).await.unwrap();
    concurrent
        .set_title("edited by concurrent CLI".into())
        .unwrap();
    ts.save(&concurrent, SnapshotSource::LocalEdit)
        .await
        .unwrap();
    let sync_before_backfill = ts.get(task.id).await.unwrap().sync;

    // Targeted backfill of the node id.
    ts.cache_remote_node_id(task.id, "I_kwDObackfilled".into())
        .await
        .unwrap();

    let after = ts.get(task.id).await.unwrap();
    assert_eq!(
        after.remote.as_ref().unwrap().node_id.as_deref(),
        Some("I_kwDObackfilled"),
        "the node id column is set by the targeted write"
    );
    assert_eq!(
        after.remote.as_ref().unwrap().remote_id,
        "100",
        "the remote_id is untouched"
    );
    assert_eq!(
        after.title, "edited by concurrent CLI",
        "the concurrent title edit survives the backfill (no whole-row clobber)"
    );
    assert_eq!(
        after.sync, sync_before_backfill,
        "the targeted backfill must not perturb sync_state"
    );

    // An absent id is a benign no-op Ok (zero rows matched).
    ts.cache_remote_node_id(domain_core::TaskId::new(), "I_ghost".into())
        .await
        .expect("cache_remote_node_id for an absent task is a no-op Ok");
}

/// rpl-4ui (Greptile review on #109) — `cache_remote_node_id` must NOT strand a
/// node id on a remote-less (local-only / draft) task. The SQLite write is
/// guarded by `remote_id IS NOT NULL`, matching the in-memory fixture's no-op,
/// so the two implementations can't diverge.
#[tokio::test]
async fn cache_remote_node_id_is_a_noop_for_a_remote_less_task() {
    let (_dir, ws, _rb, ts) = setup().await;

    let workspace = Workspace::new(WorkspaceName::new("no-remote").unwrap(), None, true);
    ws.save(&workspace).await.unwrap();

    // A local-only task — never promoted, so `remote` (and remote_id) is None.
    let task = Task::new_draft(workspace.id, None, "local only".into()).unwrap();
    ts.save(&task, SnapshotSource::LocalEdit).await.unwrap();
    assert!(ts.get(task.id).await.unwrap().remote.is_none());

    ts.cache_remote_node_id(task.id, "I_dangling".into())
        .await
        .expect("a remote-less task is a benign no-op");

    assert!(
        ts.get(task.id).await.unwrap().remote.is_none(),
        "must not strand a node id on a task with no remote"
    );
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

// ---- RFC 0002 migration-sequence integrity (#126) ---------------------------
//
// Ticket #126 owns the cross-cutting verification for the three RFC 0002
// migrations: 20260530000001 (add_filing_repo_id), 20260531000001
// (snapshot_add_filing_repo_id), and 20260601000001 (remote_mappings_rekey_filing
// — the D6 leaf-table rebuild + the section-3 backfill).
//
// The load-bearing hazard is the D6 rebuild's data-copy:
//
//     INSERT INTO remote_mappings_new (...)
//     SELECT m.task_id, COALESCE(t.filing_repo_id, t.repo_id, ''), ...
//     FROM remote_mappings m JOIN tasks t ON t.id = m.task_id;
//
// To exercise it, rows must already exist in `remote_mappings` BEFORE D6 runs.
// `setup_with_db()` / `open_from_path` apply ALL migrations (including D6)
// before any seed is possible, so a seed there is copied through an empty
// table: the rebuild's SELECT runs against zero rows and the test would pass
// even if the rebuild dropped everything (an earlier post-seed version of this
// test did exactly that — it still passed with `WHERE 1=0` injected into the
// data-copy). Instead we drive the embedded migrator manually: apply every
// migration with version < D6, seed `remote_mappings` against the pre-D6
// (repo_id-keyed) schema, then apply D6 and assert the rows survived the
// rebuild with the correct re-keyed value. A regression that drops or mis-keys
// the copy fails this test.

/// Column names of `table` via PRAGMA — used to assert schema shape.
async fn column_names(pool: &sqlx::SqlitePool, table: &str) -> Vec<String> {
    sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| sqlx::Row::get::<String, _>(&r, "name"))
        .collect()
}

/// RFC 0002 #126 — D6 rebuild data-integrity, exercised ACROSS the rebuild.
///
/// Seeds two remote-backed tasks BEFORE the D6 migration applies: one filed in
/// its own logical repo, and one CROSS-FILED (logical `repo-1`, filing
/// `repo-2`). After D6 runs, both `remote_mappings` rows must survive the
/// `INSERT...SELECT...JOIN` data-copy, the cross-filed row's key must be the
/// FILING repo (`COALESCE(filing_repo_id, repo_id)`), and `tasks.filing_repo_id`
/// must be backfilled for the row that had none. Also asserts the re-keyed
/// UNIQUE shape, the additive #115/#118 columns, and FK integrity.
#[tokio::test]
async fn rfc0002_migration_sequence_data_integrity() {
    // The D6 migration version (filename prefix 20260601000001).
    const D6_VERSION: i64 = 20260601000001;
    const TS: &str = "2026-01-01T00:00:00Z";

    let dir = TempDir::new().unwrap();
    let url = format!("sqlite://{}", dir.path().join("d6-audit.db").display());
    let pool = infra_sqlite::open_write_pool(&url)
        .await
        .expect("open write pool (no migrations yet)");

    let migrator = sqlx::migrate!("./migrations");

    // (1) Apply every migration up to — but NOT including — the D6 rebuild.
    // remote_mappings is still keyed on repo_id (the 20260527 schema) and
    // tasks.filing_repo_id exists (added by #115) but is unset.
    for m in migrator.iter() {
        if m.version < D6_VERSION {
            sqlx::raw_sql(m.sql.as_ref())
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("pre-D6 migration {} failed: {e}", m.version));
        }
    }

    // Sanity: we really are on the PRE-D6 schema (mapping keyed on repo_id), so
    // the survival assertions below genuinely test the rebuild's data-copy.
    let pre = column_names(&pool, "remote_mappings").await;
    assert!(
        pre.contains(&"repo_id".to_string()) && !pre.contains(&"filing_repo_id".to_string()),
        "pre-D6 remote_mappings must be keyed on repo_id; got {pre:?}"
    );

    // (2) Seed against the pre-D6 schema with raw SQL — the repositories write
    // the post-D6 remote_mappings shape, so they cannot seed the old columns.
    // task-a: filed in its logical repo, filing_repo_id NULL (backfill target).
    // task-b: CROSS-FILED — logical repo-1, filing repo-2 already recorded.
    // task-c: local-only, no remote mapping — exists purely so the UNIQUE
    //   subtest below has a valid FK target (see that assertion for why).
    sqlx::raw_sql(&format!(
        "INSERT INTO workspaces (id, name, status, local_only, created_at, updated_at)
           VALUES ('ws-1', 'd6-audit', 'created', 1, '{TS}', '{TS}');
         INSERT INTO repos (id, workspace_id, remote_url, canonical_url, created_at, updated_at)
           VALUES ('repo-1', 'ws-1', 'git@github.com:o/logical.git', 'github.com/o/logical', '{TS}', '{TS}'),
                  ('repo-2', 'ws-1', 'git@github.com:o/filing.git',  'github.com/o/filing',  '{TS}', '{TS}');
         INSERT INTO tasks (id, workspace_id, repo_id, title, body, status, sync_state, priority, remote_provider, remote_id, filing_repo_id, created_at, updated_at)
           VALUES ('task-a', 'ws-1', 'repo-1', 'same-filed',  '', 'done', 'synced',     'p2', 'github', '42', NULL,     '{TS}', '{TS}'),
                  ('task-b', 'ws-1', 'repo-1', 'cross-filed', '', 'done', 'synced',     'p2', 'github', '77', 'repo-2', '{TS}', '{TS}'),
                  ('task-c', 'ws-1', 'repo-1', 'no-mapping',  '', 'open', 'local_only', 'p2', NULL,     NULL, NULL,     '{TS}', '{TS}');
         INSERT INTO remote_mappings (task_id, repo_id, provider, remote_id)
           VALUES ('task-a', 'repo-1', 'github', '42'),
                  ('task-b', 'repo-1', 'github', '77');"
    ))
    .execute(&pool)
    .await
    .expect("seed pre-D6 workspace/repos/tasks/remote_mappings");

    // (3) Apply D6 and anything after it, leaving the schema current.
    for m in migrator.iter() {
        if m.version >= D6_VERSION {
            sqlx::raw_sql(m.sql.as_ref())
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("D6+ migration {} failed: {e}", m.version));
        }
    }

    // --- (a) Both rows survived the rebuild's INSERT...SELECT...JOIN. The old
    // post-seed test could not make this assertion: a data-copy that drops or
    // fails to carry rows (e.g. `WHERE 1=0`) fails HERE.
    let (mapping_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM remote_mappings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        mapping_count, 2,
        "both pre-D6 remote_mappings rows must survive the D6 rebuild"
    );

    // --- (b) The cross-filed row is re-keyed to the FILING repo, not logical.
    // Catches a rebuild that keyed on repo_id instead of
    // COALESCE(filing_repo_id, repo_id).
    let (b_key,): (String,) =
        sqlx::query_as("SELECT filing_repo_id FROM remote_mappings WHERE task_id = 'task-b'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        b_key, "repo-2",
        "cross-filed mapping must key on the FILING repo (COALESCE), not logical repo-1"
    );
    let (a_key,): (String,) =
        sqlx::query_as("SELECT filing_repo_id FROM remote_mappings WHERE task_id = 'task-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        a_key, "repo-1",
        "same-filed mapping keys on the backfilled logical repo"
    );

    // --- (c) Section-3 backfill: set tasks.filing_repo_id where NULL, and leave
    // an already-diverged value untouched.
    let (a_task,): (Option<String>,) =
        sqlx::query_as("SELECT filing_repo_id FROM tasks WHERE id = 'task-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        a_task.as_deref(),
        Some("repo-1"),
        "backfill must set tasks.filing_repo_id = repo_id for a remote-backed row that had none"
    );
    let (b_task,): (Option<String>,) =
        sqlx::query_as("SELECT filing_repo_id FROM tasks WHERE id = 'task-b'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        b_task.as_deref(),
        Some("repo-2"),
        "backfill must NOT overwrite an already-diverged filing_repo_id"
    );

    // --- (d) remote_mappings now carries filing_repo_id and dropped repo_id,
    // and the re-keyed UNIQUE(filing_repo_id, provider, remote_id) is enforced.
    let cols = column_names(&pool, "remote_mappings").await;
    assert!(
        cols.contains(&"filing_repo_id".to_string()),
        "remote_mappings must have filing_repo_id after D6; got {cols:?}"
    );
    assert!(
        !cols.contains(&"repo_id".to_string()),
        "remote_mappings must NOT retain repo_id after D6; got {cols:?}"
    );
    // Use task-c's id: it is a real tasks row (FK satisfied) with no existing
    // mapping, so ONLY the UNIQUE(filing_repo_id, provider, remote_id) clause
    // can fail here. A non-existent task_id would also trip the task_id FK, and
    // "FOREIGN KEY constraint failed" contains "constraint" — the assertion
    // would then pass even if the UNIQUE clause were missing.
    let dup = sqlx::query(
        "INSERT INTO remote_mappings (task_id, filing_repo_id, provider, remote_id) \
         VALUES ('task-c', 'repo-1', 'github', '42')",
    )
    .execute(&pool)
    .await;
    let err =
        dup.expect_err("a duplicate (filing_repo_id, provider, remote_id) must violate UNIQUE");
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("unique"),
        "expected a UNIQUE constraint violation specifically, got: {err}"
    );

    // --- (e) Additive nullable columns from #115 / #118.
    for (table, col) in &[
        ("workspaces", "filing_repo_id"),
        ("tasks", "filing_repo_id"),
        ("task_snapshots", "filing_repo_id"),
    ] {
        let tcols = column_names(&pool, table).await;
        assert!(
            tcols.contains(&col.to_string()),
            "{table} must have the additive {col} column; got {tcols:?}"
        );
    }

    // --- (f) No FK orphans after the full sequence + seed.
    let orphans: Vec<String> = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| {
            let table: String = sqlx::Row::get(&r, "table");
            let rowid: i64 = sqlx::Row::get(&r, "rowid");
            let parent: String = sqlx::Row::get(&r, "parent");
            format!("{table} rowid={rowid} → {parent}")
        })
        .collect();
    assert!(
        orphans.is_empty(),
        "PRAGMA foreign_key_check must find no orphans after the RFC 0002 sequence; got {orphans:?}"
    );

    drop(dir);
}

/// RFC 0005 §D6 tripwires for the shared-repo-identity migration
/// (`20260629000001`). Seeds the PRE-migration duplicate-repo shape — the exact
/// bug #202 fixes: the same `canonical_url` attached to two workspaces with
/// divergent prefix/name/aliases, plus two tasks mirroring the same remote issue
/// into it — then runs the migration and asserts the transformation. Mirrors
/// `rfc0002_migration_sequence_data_integrity`. These pin SQLite behaviours the
/// migration relies on (rename carries child FKs; RENAME COLUMN rewrites the
/// expression index) and the data rules (survivor, alias-fold, collision dedup).
#[tokio::test]
async fn rfc0005_migration_splits_identity_keeps_instances_and_dedups_remote() {
    const D5_VERSION: i64 = 20260629000001;
    const TS1: &str = "2026-01-01T00:00:00Z"; // earlier — the survivor
    const TS2: &str = "2026-02-01T00:00:00Z"; // later

    let dir = TempDir::new().unwrap();
    let url = format!("sqlite://{}", dir.path().join("rfc0005-audit.db").display());
    let pool = infra_sqlite::open_write_pool(&url)
        .await
        .expect("open write pool (no migrations yet)");
    let migrator = sqlx::migrate!("./migrations");

    // (1) Apply every migration BEFORE the RFC 0005 split → the pre-split schema.
    for m in migrator.iter() {
        if m.version < D5_VERSION {
            sqlx::raw_sql(m.sql.as_ref())
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("pre-0005 migration {} failed: {e}", m.version));
        }
    }
    let pre = column_names(&pool, "repos").await;
    assert!(
        pre.contains(&"prefix".to_string()),
        "pre-0005 repos must still carry the per-workspace prefix; got {pre:?}"
    );

    // (2) Seed the duplicate-repo bug: one canonical, two workspaces, divergent
    // prefix (shr/shr1) + name + aliases; two tasks mirroring the SAME remote
    // issue (github/500) into it — a cross-workspace remote_mappings collision.
    sqlx::raw_sql(&format!(
        "INSERT INTO workspaces (id, name, status, local_only, created_at, updated_at)
           VALUES ('ws-1','a','created',1,'{TS1}','{TS1}'), ('ws-2','b','created',1,'{TS2}','{TS2}');
         INSERT INTO repos (id, workspace_id, remote_url, canonical_url, tracked_branch, name, aliases, prefix, created_at, updated_at)
           VALUES ('inst-1','ws-1','git@github.com:o/shared.git','github.com/o/shared',NULL,'shared','[\"legacy\"]','shr','{TS1}','{TS1}'),
                  ('inst-2','ws-2','git@github.com:o/shared.git','github.com/o/shared',NULL,'shared-renamed','[]','shr1','{TS2}','{TS2}');
         INSERT INTO tasks (id, workspace_id, repo_id, title, body, status, sync_state, priority, remote_provider, remote_id, filing_repo_id, created_at, updated_at)
           VALUES ('task-1','ws-1','inst-1','t1','','done','synced','p2','github','500','inst-1','{TS1}','{TS1}'),
                  ('task-2','ws-2','inst-2','t2','','done','synced','p2','github','500','inst-2','{TS2}','{TS2}');
         INSERT INTO remote_mappings (task_id, filing_repo_id, provider, remote_id, last_synced_at)
           VALUES ('task-1','inst-1','github','500','{TS1}'),
                  ('task-2','inst-2','github','500','{TS2}');"
    ))
    .execute(&pool)
    .await
    .expect("seed pre-0005 duplicate-repo state");

    // (3) Apply the RFC 0005 migration (and anything after it).
    for m in migrator.iter() {
        if m.version >= D5_VERSION {
            sqlx::raw_sql(m.sql.as_ref())
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("0005+ migration {} failed: {e}", m.version));
        }
    }

    // --- (a) The duplicated canonical collapses to ONE shared origin; the
    // earliest-created instance's prefix wins, and the non-surviving instance's
    // name + aliases are folded in (survivor rule, §D6 step 3).
    let (origin_id, prefix, aliases): (String, String, String) = sqlx::query_as(
        "SELECT id, prefix, aliases FROM repo_origins WHERE canonical_url = 'github.com/o/shared'",
    )
    .fetch_one(&pool)
    .await
    .expect("exactly one origin for the shared canonical");
    assert_eq!(
        prefix, "shr",
        "earliest-created instance's prefix wins (not shr1)"
    );
    assert!(
        aliases.contains("legacy"),
        "unioned alias preserved; got {aliases}"
    );
    assert!(
        aliases.contains("shared-renamed"),
        "non-surviving instance name folded into origin aliases; got {aliases}"
    );

    // --- (b) BOTH per-workspace instances are kept, sharing the one origin.
    let (inst_count, distinct_origins): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COUNT(DISTINCT origin_id) FROM repo_instances WHERE canonical_url = 'github.com/o/shared'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(inst_count, 2, "every instance is kept (none merged away)");
    assert_eq!(
        distinct_origins, 1,
        "both instances share one origin_id => consistent prefix across workspaces"
    );

    // --- (c) tasks.repo_id renamed to repo_instance_id.
    let tcols = column_names(&pool, "tasks").await;
    assert!(
        tcols.contains(&"repo_instance_id".to_string()) && !tcols.contains(&"repo_id".to_string()),
        "tasks.repo_id must be renamed to repo_instance_id; got {tcols:?}"
    );

    // --- (d) child FKs followed the table rename to repo_instances.
    for child in ["tasks", "worktree_links"] {
        let parents: Vec<String> = sqlx::query(&format!(
            "SELECT \"table\" AS p FROM pragma_foreign_key_list('{child}')"
        ))
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| sqlx::Row::get::<String, _>(&r, "p"))
        .collect();
        assert!(
            parents.iter().any(|p| p == "repo_instances"),
            "{child} FK must target repo_instances after the rename; got {parents:?}"
        );
    }

    // --- (e) remote-identity index keyed on filing_repo_id ALONE — the
    // COALESCE(filing_repo_id, repo_id) fallback is gone (origin id space, §D4).
    let (idx_sql,): (String,) =
        sqlx::query_as("SELECT sql FROM sqlite_master WHERE name = 'idx_tasks_remote_lookup'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        idx_sql.contains("filing_repo_id"),
        "remote-lookup index must key on filing_repo_id; got {idx_sql}"
    );
    assert!(
        !idx_sql.to_uppercase().contains("COALESCE"),
        "COALESCE fallback must be dropped (origin id space only); got {idx_sql}"
    );

    // --- (f) the two cross-workspace mappings for the same remote issue collapse
    // to ONE (collision dedup, §D6 step 7d) re-keyed to the shared origin id.
    let (mapping_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM remote_mappings WHERE provider = 'github' AND remote_id = '500'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        mapping_count, 1,
        "colliding cross-workspace mappings must dedup to one before the origin rewrite"
    );
    let (survivor_filing,): (String,) = sqlx::query_as(
        "SELECT filing_repo_id FROM remote_mappings WHERE provider = 'github' AND remote_id = '500'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        survivor_filing, origin_id,
        "surviving mapping must be re-keyed to the shared origin id"
    );

    // --- (g) no FK orphans after the full sequence.
    let orphans: Vec<String> = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| sqlx::Row::get::<String, _>(&r, "table"))
        .collect();
    assert!(
        orphans.is_empty(),
        "PRAGMA foreign_key_check must find no orphans after the RFC 0005 sequence; got {orphans:?}"
    );

    drop(dir);
}

/// rpl-sv2 follow-up: `find_by_remote_mapping` must be
/// workspace-scoped and ambiguous (≥2 matches in the same workspace)
/// must surface as `None` (so the doctor surfaces the situation as
/// `unresolved` rather than arbitrarily picking a binding during
/// `--repair`). CodeRabbit review flagged the prior `LIMIT 1` cut
/// as a silent-divergence miss.
#[tokio::test]
async fn find_by_remote_mapping_is_workspace_scoped_and_ambiguity_returns_none() {
    let (_dir, ws, rb, ts) = setup().await;

    // Two workspaces, each with one binding that holds the same
    // `(provider, remote_id)` — a cross-workspace import shape.
    let w1 = Workspace::new(WorkspaceName::new("w1").unwrap(), None, true);
    let w2 = Workspace::new(WorkspaceName::new("w2").unwrap(), None, true);
    ws.save(&w1).await.unwrap();
    ws.save(&w2).await.unwrap();

    // Each origin must have a unique prefix — same-shaped canonicals
    // would otherwise collide on the `repo_origins.prefix` UNIQUE index.
    let repo_w1 = seed_binding(
        &rb,
        w1.id,
        "git@github.com:o/cross-w1.git",
        "github.com/o/cross-w1",
        Some("cw1"),
    )
    .await;
    let repo_w2 = seed_binding(
        &rb,
        w2.id,
        "git@github.com:o/cross-w2.git",
        "github.com/o/cross-w2",
        Some("cw2"),
    )
    .await;
    // RFC 0005: find_by_remote_mapping resolves to ORIGIN ids.
    let repo_w1_origin = RepoOriginId::from_uuid(repo_w1.origin_id.as_uuid());
    let repo_w2_origin = RepoOriginId::from_uuid(repo_w2.origin_id.as_uuid());

    // One task per workspace, both mirroring the same github issue
    // (legitimate cross-workspace import — different workspaces
    // importing the same upstream issue for local tracking). Each
    // records its own logical repo's origin as the filing repo so the
    // `remote_mappings` row is keyed in ORIGIN id space (#D4).
    for instance in [&repo_w1, &repo_w2] {
        let mut t = Task::new_draft(
            instance.workspace_id,
            Some(instance.id),
            "shared issue".into(),
        )
        .unwrap();
        t.set_filing_repo_id(Some(RepoId::from_uuid(instance.origin_id.as_uuid())))
            .unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", "shared-1"))
            .unwrap();
        ts.save(&t, SnapshotSource::Promote).await.unwrap();
    }

    // Cross-workspace lookup must be scoped: w1's lookup must NOT
    // return w2's binding. (If it did, `rl repo doctor --repair`
    // would silently re-point a w1 task to a w2 binding — a
    // cross-workspace data corruption, the exact silent-divergence
    // class rpl-sv2 exists to heal.)
    let w1_hit = rb
        .find_by_remote_mapping(w1.id, "github", "shared-1")
        .await
        .unwrap();
    assert_eq!(
        w1_hit,
        Some(repo_w1_origin),
        "w1 lookup must return the w1 origin, not a cross-workspace pick"
    );
    let w2_hit = rb
        .find_by_remote_mapping(w2.id, "github", "shared-1")
        .await
        .unwrap();
    assert_eq!(w2_hit, Some(repo_w2_origin));

    // Ambiguity-in-workspace case: two origins in w1 with the
    // same `(provider, remote_id)` row. (Reach it by saving a
    // second task in w1 pointing at a *different* origin under
    // the same remote_id — `remote_mappings` carries a UNIQUE on
    // `(filing_repo_id, provider, remote_id)`, so two distinct
    // origins in the same workspace CAN both hold the same
    // `(provider, remote_id)` row.)
    let repo_w1_2 = seed_binding(
        &rb,
        w1.id,
        "git@github.com:o/cross-w1-mirror.git",
        "github.com/o/cross-w1-mirror",
        Some("cw1m"),
    )
    .await;
    let mut t2 = Task::new_draft(w1.id, Some(repo_w1_2.id), "ambiguous".into()).unwrap();
    t2.set_filing_repo_id(Some(RepoId::from_uuid(repo_w1_2.origin_id.as_uuid())))
        .unwrap();
    t2.stage_for_sync().unwrap();
    t2.promote_to_remote(RemoteRef::new("github", "shared-1"))
        .unwrap();
    ts.save(&t2, SnapshotSource::Promote).await.unwrap();

    // Now w1's lookup must surface ambiguity → return `None`
    // (doctor reports `unresolved`, user picks `--target`).
    let ambiguous = rb
        .find_by_remote_mapping(w1.id, "github", "shared-1")
        .await
        .unwrap();
    assert_eq!(
        ambiguous, None,
        "ambiguous workspace lookup must return None (not arbitrarily pick)"
    );
    // w2's lookup is unaffected (only one origin holds the
    // mapping there).
    let w2_unchanged = rb
        .find_by_remote_mapping(w2.id, "github", "shared-1")
        .await
        .unwrap();
    assert_eq!(w2_unchanged, Some(repo_w2_origin));
}

/// RFC 0004 D1 invariant — *blocked_by referential integrity*. The storage
/// layer is the primary enforcement: `task_relations.other_task_id` carries an
/// FK → `tasks(id)`, so persisting a relation whose target task does not exist
/// is rejected. (The aggregate `add_relation` is offline and does not validate
/// the target — the DB FK is the safety net, per the RFC.)
#[tokio::test]
async fn blocked_by_to_nonexistent_task_is_rejected_by_fk() {
    let (_dir, ws, _rb, ts) = setup().await;
    let w = Workspace::new(WorkspaceName::new("w").unwrap(), None, true);
    ws.save(&w).await.unwrap();

    let mut t = Task::new_draft(w.id, None, "blocked task".into()).unwrap();
    // Point the blocker at a TaskId that was never persisted.
    t.add_relation(RelationKind::BlockedBy, domain_core::TaskId::new());

    let err = ts
        .save(&t, SnapshotSource::Created)
        .await
        .expect_err("a relation to a non-existent task must violate the FK");
    let msg = format!("{err:?}").to_lowercase();
    assert!(
        msg.contains("foreign") || msg.contains("constraint"),
        "expected a foreign-key violation, got: {err:?}"
    );
}
