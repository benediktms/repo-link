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

fn init_git_repo_with_origin(path: &std::path::Path, url: &str) {
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(path)
        .status()
        .expect("git init");
    assert!(status.success());
    let status = std::process::Command::new("git")
        .args(["remote", "add", "origin", url])
        .current_dir(path)
        .status()
        .expect("git remote add");
    assert!(status.success());
}

#[test]
fn repo_and_worktree_lifecycle() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();

    // Create a temp git repo with the matching origin so the attach + link
    // canonical checks pass.
    let repo_dir = TempDir::new().unwrap();
    init_git_repo_with_origin(repo_dir.path(), "git@github.com:o/r.git");
    let repo_path = repo_dir.path().display().to_string();

    let outcome = run_json(
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
            "--path",
            &repo_path,
        ],
    );
    let repo_id = outcome["binding"]["id"].as_str().unwrap().to_string();
    // The attach linked the worktree (repo_path), so worktrees is non-empty.
    assert!(!outcome["binding"]["worktrees"].as_array().unwrap().is_empty());

    // Link a second worktree (different checkout of same origin).
    let second_dir = TempDir::new().unwrap();
    init_git_repo_with_origin(second_dir.path(), "git@github.com:o/r.git");
    let second_path = second_dir.path().display().to_string();

    let linked = run_json(
        &mut bin("repo-link", &dir),
        &[
            "worktree",
            "link",
            "--repo",
            &repo_id,
            "--path",
            &second_path,
            "--branch",
            "main",
        ],
    );
    assert_eq!(linked["worktrees"].as_array().unwrap().len(), 2);
    assert!(linked["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .all(|w| w["status"] == "linked"));

    let unlinked = run_json(
        &mut bin("repo-link", &dir),
        &["worktree", "unlink", "--repo", &repo_id, "--path", &second_path],
    );
    assert_eq!(unlinked["worktrees"].as_array().unwrap().len(), 1);
}

/// Tombstone invariant: even when the linked path's *leaf* has been deleted
/// (so `canonicalize` of the user's input fails), unlinking with the same
/// input string must still find the stored entry — as long as a *prefix*
/// of the input still exists and can resolve any symlinks. Specifically
/// covers the case the reviewer's partial fix wouldn't catch: an absolute
/// path with a symlinked parent (e.g. macOS `/var → /private/var`).
#[cfg(unix)]
#[test]
fn worktree_unlink_tombstoned_path_with_symlinked_prefix_resolves() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Filesystem shape:
    //   tmpdir/real_root/child/.git  (a real git repo with origin)
    //   tmpdir/alias_root          -> real_root (symlink)
    // User links via the symlinked path; CLI canonicalises and stores the
    // real path. Then the leaf (`child`) is deleted, and unlink is called
    // with the original symlinked input — canonicalize fails on the input
    // (leaf gone) but the parent `alias_root` is still a live symlink.
    let real_root = dir.path().join("real_root");
    let child = real_root.join("child");
    std::fs::create_dir_all(&child).unwrap();
    init_git_repo_with_origin(&child, "git@github.com:o/r.git");

    let alias_root = dir.path().join("alias_root");
    symlink(&real_root, &alias_root).unwrap();

    let user_path = alias_root.join("child").display().to_string();

    let repo_id = run_json(
        &mut bin("repo-link", &dir),
        &[
            "repo", "attach", "--workspace", &workspace,
            "--url", "git@github.com:o/r.git",
            "--canonical", "github.com/o/r",
            "--no-link",
        ],
    )["binding"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Link via the symlinked input. Stored path should be the canonical form.
    bin("repo-link", &dir)
        .args(["worktree", "link", "--repo", &repo_id, "--path", &user_path])
        .assert()
        .success();
    let stored = run_json(&mut bin("repo-link", &dir), &["repo", "show", &repo_id])
        ["worktrees"][0]["path"]
        .as_str()
        .unwrap()
        .to_string();
    let expected_canonical = std::fs::canonicalize(&child).unwrap().display().to_string();
    assert_eq!(stored, expected_canonical, "link should store canonical form");

    // Delete the target so canonicalize(user_path) will fail.
    std::fs::remove_dir_all(&child).unwrap();

    // Unlink with the *same* user input. Must succeed.
    let unlinked = run_json(
        &mut bin("repo-link", &dir),
        &["worktree", "unlink", "--repo", &repo_id, "--path", &user_path],
    );
    assert!(
        unlinked["worktrees"].as_array().unwrap().is_empty(),
        "unlink with same input as link must succeed even when leaf is gone; \
         got worktrees: {:?}",
        unlinked["worktrees"]
    );
}

/// Invariant: `worktree link --path X` followed by `worktree unlink --path X`
/// with the *exact same input string* X must round-trip — the worktree should
/// be gone. Before the canonicalisation fix, link persisted the canonical
/// form while unlink looked up the raw form, so on platforms where
/// `canonicalize` rewrites the prefix (macOS `/var` → `/private/var`) the
/// unlink couldn't find the entry.
#[test]
fn worktree_link_unlink_round_trips_with_same_input() {
    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    let repo_dir = TempDir::new().unwrap();
    init_git_repo_with_origin(repo_dir.path(), "git@github.com:o/r.git");
    // Raw path as the user would type it — on macOS this is the symlink
    // form (`/var/folders/...`), not the canonical (`/private/var/...`).
    let path = repo_dir.path().display().to_string();

    let repo_id = run_json(
        &mut bin("repo-link", &dir),
        &[
            "repo", "attach", "--workspace", &workspace,
            "--url", "git@github.com:o/r.git",
            "--canonical", "github.com/o/r",
            "--no-link",
        ],
    )["binding"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    bin("repo-link", &dir)
        .args(["worktree", "link", "--repo", &repo_id, "--path", &path])
        .assert()
        .success();

    let unlinked = run_json(
        &mut bin("repo-link", &dir),
        &["worktree", "unlink", "--repo", &repo_id, "--path", &path],
    );
    assert!(
        unlinked["worktrees"].as_array().unwrap().is_empty(),
        "link/unlink with identical --path input must round-trip; got worktrees: {:?}",
        unlinked["worktrees"]
    );
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

    // Attach with --no-link so we can control worktree registration below.
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
            "--no-link",
        ],
    )["binding"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // alive_dir is a real git repo with the matching origin.
    // We compare assertions against the canonical form because the CLI
    // canonicalises before persisting (e.g. macOS rewrites `/var` →
    // `/private/var`).
    let alive_dir = TempDir::new().unwrap();
    init_git_repo_with_origin(alive_dir.path(), "git@github.com:o/r.git");
    let alive = std::fs::canonicalize(alive_dir.path()).unwrap().display().to_string();

    // gone is a path that doesn't exist yet; create a git repo there first so
    // the link canonical check passes, then the reconcile will see it missing.
    let gone_dir = TempDir::new().unwrap();
    init_git_repo_with_origin(gone_dir.path(), "git@github.com:o/r.git");
    let gone = std::fs::canonicalize(gone_dir.path()).unwrap().display().to_string();

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

    // Drop the gone_dir so the path disappears from the filesystem.
    drop(gone_dir);

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
#[cfg(unix)]
fn gh_auth_writes_secure_file_and_blocks_sync_when_loosened() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let token_file = dir.path().join("github_token");

    let mut cmd = bin("rl", &dir);
    cmd.env("REPO_LINK_GITHUB_TOKEN_FILE", &token_file);
    cmd.env_remove("REPO_LINK_GITHUB_TOKEN");
    cmd.env_remove("GITHUB_TOKEN");
    let result = run_json(&mut cmd, &["gh", "auth", "--token", "abc123"]);
    // Use canonicalize so symlinks (e.g. /var → /private/var on macOS) don't
    // cause a mismatch between the path the binary resolves and what we built.
    let canonical_token_file = token_file.canonicalize().unwrap_or_else(|_| token_file.clone());
    assert_eq!(result["file"], canonical_token_file.display().to_string());
    assert_eq!(result["mode"], "0600");

    let mode = std::fs::metadata(&token_file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(std::fs::read_to_string(&token_file).unwrap().trim(), "abc123");

    // Loosen permissions and assert any sync command rejects it.
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut sync_cmd = bin("rl", &dir);
    sync_cmd.env("REPO_LINK_GITHUB_TOKEN_FILE", &token_file);
    sync_cmd.env_remove("REPO_LINK_GITHUB_TOKEN");
    sync_cmd.env_remove("GITHUB_TOKEN");
    let output = sync_cmd
        .args(["sync", "push", "--task", "00000000-0000-0000-0000-000000000000"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("insecure permissions"),
        "expected insecure-permissions error; got: {stderr}"
    );
    assert!(stderr.contains("0644"), "expected mode in error; got: {stderr}");
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
