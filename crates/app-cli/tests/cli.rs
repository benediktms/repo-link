use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

fn bin(name: &str, dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin(name).expect("bin");
    cmd.env("REPO_LINK_DB", dir.path().join("repo-link.db"));
    cmd
}

fn run_json(cmd: &mut Command, args: &[&str]) -> Value {
    let output = cmd.args(args).assert().success().get_output().clone();
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {stdout}"))
}

#[test]
fn workspace_create_show_list_roundtrip() {
    let dir = TempDir::new().unwrap();
    let created = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "scratch", "--local-only"],
    );
    let id = created["id"].as_str().expect("id").to_string();
    assert_eq!(created["status"], "created");
    assert_eq!(created["local_only"], true);

    let listed = run_json(&mut bin("repo-link", &dir), &["workspace", "list"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let shown = run_json(&mut bin("repo-link", &dir), &["workspace", "show", &id]);
    assert_eq!(shown["id"], id);
}

#[test]
fn task_create_list_includes_state_filter() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();

    let task = run_json(
        &mut bin("repo-link", &dir),
        &[
            "task",
            "create",
            "--workspace",
            &workspace,
            "--title",
            "ship it",
            "--priority",
            "p1",
        ],
    );
    let task_id = task["id"].as_str().unwrap().to_string();
    assert_eq!(task["status"], "open");
    assert_eq!(task["sync_state"], "local_only");

    // Stage takes Vec<task ids> now → returns a batch array.
    let staged_batch = run_json(&mut bin("repo-link", &dir), &["task", "stage", &task_id]);
    assert_eq!(staged_batch.as_array().unwrap().len(), 1);
    assert_eq!(staged_batch[0]["ok"], true);
    assert_eq!(staged_batch[0]["task"]["sync_state"], "staged");

    // Filter list by sync_state (not status — status is still `open` for both).
    let local_only = run_json(
        &mut bin("repo-link", &dir),
        &["task", "list", "--sync-state", "local_only"],
    );
    assert!(local_only.as_array().unwrap().is_empty());
    let staged_list = run_json(
        &mut bin("repo-link", &dir),
        &["task", "list", "--sync-state", "staged"],
    );
    assert_eq!(staged_list.as_array().unwrap().len(), 1);
}

#[test]
fn task_batch_lifecycle_commands_emit_per_task_results() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();

    let mut ids: Vec<String> = Vec::new();
    for n in 0..3 {
        let t = run_json(
            &mut bin("repo-link", &dir),
            &[
                "task",
                "create",
                "--workspace",
                &workspace,
                "--title",
                &format!("t{n}"),
            ],
        );
        ids.push(t["id"].as_str().unwrap().to_string());
    }

    // Start all three; expect three per-task success entries.
    let mut args = vec!["task".to_string(), "start".to_string()];
    args.extend(ids.iter().cloned());
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let started = run_json(&mut bin("repo-link", &dir), &arg_refs);
    let rows = started.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    for row in rows {
        assert_eq!(row["ok"], true);
        assert_eq!(row["task"]["status"], "in_progress");
    }
}

#[test]
fn repo_and_worktree_lifecycle() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();

    let repo = run_json(
        &mut bin("repo-link", &dir),
        &[
            "repo",
            "attach",
            "--workspace",
            &workspace,
            "--url",
            "git@github.com:o/r.git",
            "--canonical",
            "github.com/o/r",
            "--branch",
            "main",
        ],
    );
    let repo_id = repo["id"].as_str().unwrap().to_string();
    assert!(repo["worktrees"].as_array().unwrap().is_empty());

    let linked = run_json(
        &mut bin("repo-link", &dir),
        &[
            "worktree",
            "link",
            "--repo",
            &repo_id,
            "--path",
            "/tmp/r",
            "--branch",
            "main",
        ],
    );
    assert_eq!(linked["worktrees"].as_array().unwrap().len(), 1);
    assert_eq!(linked["worktrees"][0]["status"], "linked");

    let unlinked = run_json(
        &mut bin("repo-link", &dir),
        &["worktree", "unlink", "--repo", &repo_id, "--path", "/tmp/r"],
    );
    assert!(unlinked["worktrees"].as_array().unwrap().is_empty());
}

#[test]
fn query_overview_reports_counts() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();

    for n in 0..3 {
        bin("repo-link", &dir)
            .args([
                "task",
                "create",
                "--workspace",
                &workspace,
                "--title",
                &format!("t{n}"),
            ])
            .assert()
            .success();
    }

    let ov = run_json(
        &mut bin("repo-link", &dir),
        &["query", "overview", "--workspace", &workspace],
    );
    assert_eq!(ov["repo_count"], 0);
    assert_eq!(ov["by_status"]["open"], 3);
    assert_eq!(ov["by_sync"]["local_only"], 3);
    assert_eq!(ov["unsynced_task_count"], 3);
}

#[test]
fn rl_alias_is_a_working_binary() {
    let dir = TempDir::new().unwrap();
    bin("repo-link", &dir)
        .args(["workspace", "create", "viaroot", "--local-only"])
        .assert()
        .success();

    let listed = run_json(&mut bin("rl", &dir), &["workspace", "list"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["name"], "viaroot");
}

#[test]
fn worktree_reconcile_marks_missing_against_real_fs() {
    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let repo_id = run_json(
        &mut bin("repo-link", &dir),
        &[
            "repo",
            "attach",
            "--workspace",
            &workspace,
            "--url",
            "git@github.com:o/r.git",
            "--canonical",
            "github.com/o/r",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    let alive_dir = TempDir::new().unwrap();
    let alive = alive_dir.path().display().to_string();
    let gone = "/tmp/repo-link-never-exists-zzz".to_string();

    bin("repo-link", &dir)
        .args([
            "worktree", "link", "--repo", &repo_id, "--path", &alive, "--branch", "main",
        ])
        .assert()
        .success();
    bin("repo-link", &dir)
        .args(["worktree", "link", "--repo", &repo_id, "--path", &gone])
        .assert()
        .success();

    let summary = run_json(
        &mut bin("repo-link", &dir),
        &["worktree", "reconcile", "--workspace", &workspace],
    );
    assert_eq!(summary["repos_checked"], 1);
    assert_eq!(summary["worktrees_checked"], 2);
    assert_eq!(summary["marked_missing"], 1);
    assert_eq!(summary["pruned"], 0);

    let show = run_json(&mut bin("repo-link", &dir), &["repo", "show", &repo_id]);
    let by_path: std::collections::HashMap<String, String> = show["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| (w["path"].as_str().unwrap().to_string(), w["status"].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(by_path[&alive], "linked");
    assert_eq!(by_path[&gone], "missing_path");

    let summary2 = run_json(
        &mut bin("repo-link", &dir),
        &["worktree", "reconcile", "--workspace", &workspace, "--prune"],
    );
    assert_eq!(summary2["pruned"], 1);
    let show_after = run_json(&mut bin("repo-link", &dir), &["repo", "show", &repo_id]);
    assert_eq!(show_after["worktrees"].as_array().unwrap().len(), 1);
}

#[test]
fn task_snapshots_lists_history_after_edits() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();

    let task = run_json(
        &mut bin("repo-link", &dir),
        &["task", "create", "--workspace", &workspace, "--title", "original"],
    );
    let task_id = task["id"].as_str().unwrap().to_string();

    // Start the task — produces a second snapshot.
    bin("repo-link", &dir)
        .args(["task", "start", &task_id])
        .assert()
        .success();

    let snaps = run_json(
        &mut bin("repo-link", &dir),
        &["task", "snapshots", &task_id],
    );
    let arr = snaps.as_array().expect("snapshots is array");
    assert!(arr.len() >= 2, "expected ≥2 snapshots, got {}", arr.len());
    assert_eq!(arr[0]["version"], 1);
}

#[test]
fn task_rollback_restores_previous_title() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();

    let task = run_json(
        &mut bin("repo-link", &dir),
        &["task", "create", "--workspace", &workspace, "--title", "v1 title"],
    );
    let task_id = task["id"].as_str().unwrap().to_string();

    // A second mutation so version 2 exists (start changes status → new snapshot).
    bin("repo-link", &dir)
        .args(["task", "start", &task_id])
        .assert()
        .success();

    // Roll back to version 1.
    let rolled = run_json(
        &mut bin("repo-link", &dir),
        &["task", "rollback", &task_id, "--to-version", "1"],
    );
    assert_eq!(rolled["title"], "v1 title");
}

#[test]
fn batch_failure_exits_nonzero() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();

    let task = run_json(
        &mut bin("repo-link", &dir),
        &["task", "create", "--workspace", &workspace, "--title", "real task"],
    );
    let valid_id = task["id"].as_str().unwrap().to_string();
    let invalid_id = "00000000-0000-0000-0000-000000000000".to_string();

    // Mix one valid ID and one invalid ID — the batch should fail (nonzero exit)
    // but still print the full JSON array on stdout.
    let output = bin("repo-link", &dir)
        .args(["task", "start", &valid_id, &invalid_id])
        .assert()
        .failure()
        .get_output()
        .clone();

    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    let batch: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {stdout}"));

    let rows = batch.as_array().expect("expected JSON array on stdout");
    assert_eq!(rows.len(), 2, "expected both rows even on partial failure");

    let ok_row = rows.iter().find(|r| r["task_id"] == valid_id).expect("valid id row");
    assert_eq!(ok_row["ok"], true);

    let err_row = rows.iter().find(|r| r["task_id"] == invalid_id).expect("invalid id row");
    assert_eq!(err_row["ok"], false);
}

#[test]
fn invalid_priority_exits_nonzero_with_readable_error() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();

    let output = bin("repo-link", &dir)
        .args([
            "task",
            "create",
            "--workspace",
            &workspace,
            "--title",
            "t",
            "--priority",
            "P99",
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.to_lowercase().contains("priority"), "stderr: {stderr}");
}
