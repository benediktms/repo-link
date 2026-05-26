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
    assert!(
        !outcome["binding"]["worktrees"]
            .as_array()
            .unwrap()
            .is_empty()
    );

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
    assert!(
        linked["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["status"] == "linked")
    );

    let unlinked = run_json(
        &mut bin("repo-link", &dir),
        &[
            "worktree",
            "unlink",
            "--repo",
            &repo_id,
            "--path",
            &second_path,
        ],
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

    // Link via the symlinked input. Stored path should be the canonical form.
    bin("repo-link", &dir)
        .args(["worktree", "link", "--repo", &repo_id, "--path", &user_path])
        .assert()
        .success();
    let stored = run_json(&mut bin("repo-link", &dir), &["repo", "show", &repo_id])["worktrees"][0]
        ["path"]
        .as_str()
        .unwrap()
        .to_string();
    let expected_canonical = std::fs::canonicalize(&child).unwrap().display().to_string();
    assert_eq!(
        stored, expected_canonical,
        "link should store canonical form"
    );

    // Delete the target so canonicalize(user_path) will fail.
    std::fs::remove_dir_all(&child).unwrap();

    // Unlink with the *same* user input. Must succeed.
    let unlinked = run_json(
        &mut bin("repo-link", &dir),
        &[
            "worktree", "unlink", "--repo", &repo_id, "--path", &user_path,
        ],
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
fn worktree_link_error_suggests_single_repo_hint_when_canonical_matches_one_binding() {
    // Regression guard: single-match case must keep the existing
    // "use --repo X instead" message after the multi-match refactor.
    let dir = TempDir::new().unwrap();

    let ws_a = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "ws-a", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let ws_target = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "ws-target", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    let b_a = attach_no_link(
        &dir,
        &ws_a,
        "git@github.com:o/shared.git",
        "github.com/o/shared",
    );
    let b_target = attach_no_link(
        &dir,
        &ws_target,
        "git@github.com:o/other.git",
        "github.com/o/other",
    );

    let repo_dir = TempDir::new().unwrap();
    init_git_repo_with_origin(repo_dir.path(), "git@github.com:o/shared.git");
    let path = repo_dir.path().display().to_string();

    let output = bin("repo-link", &dir)
        .args(["worktree", "link", "--repo", &b_target, "--path", &path])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert!(
        stderr.contains(&format!("use --repo {b_a} instead")),
        "single-match path should suggest the lone binding; got: {stderr}"
    );
}

#[test]
fn worktree_link_error_lists_all_candidates_when_canonical_matches_multiple_bindings() {
    // The bug being fixed: when `discovered` canonical maps to bindings in
    // multiple workspaces, `.first()` arbitrarily picks one. We want the
    // error to surface ALL candidates so the user can pick correctly.
    let dir = TempDir::new().unwrap();

    let ws_a = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "ws-a", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let ws_b = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "ws-b", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let ws_target = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "ws-target", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Same canonical, two different workspaces.
    let b_a = attach_no_link(
        &dir,
        &ws_a,
        "git@github.com:o/shared.git",
        "github.com/o/shared",
    );
    let b_b = attach_no_link(
        &dir,
        &ws_b,
        "git@github.com:o/shared.git",
        "github.com/o/shared",
    );
    // Decoy binding the user is mistakenly passing as --repo.
    let b_target = attach_no_link(
        &dir,
        &ws_target,
        "git@github.com:o/other.git",
        "github.com/o/other",
    );

    let repo_dir = TempDir::new().unwrap();
    init_git_repo_with_origin(repo_dir.path(), "git@github.com:o/shared.git");
    let path = repo_dir.path().display().to_string();

    let output = bin("repo-link", &dir)
        .args(["worktree", "link", "--repo", &b_target, "--path", &path])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert!(
        stderr.contains(&b_a),
        "stderr should mention ws-a binding ({b_a}); got: {stderr}"
    );
    assert!(
        stderr.contains(&b_b),
        "stderr should mention ws-b binding ({b_b}); got: {stderr}"
    );
    assert!(
        stderr.contains("ws-a") && stderr.contains("ws-b"),
        "stderr should name both workspaces so the user can disambiguate; got: {stderr}"
    );
    assert!(
        stderr.contains("multiple") || stderr.contains("choose --repo"),
        "stderr should signal that the user must pick; got: {stderr}"
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
    let alive = std::fs::canonicalize(alive_dir.path())
        .unwrap()
        .display()
        .to_string();

    // gone is a path that doesn't exist yet; create a git repo there first so
    // the link canonical check passes, then the reconcile will see it missing.
    let gone_dir = TempDir::new().unwrap();
    init_git_repo_with_origin(gone_dir.path(), "git@github.com:o/r.git");
    let gone = std::fs::canonicalize(gone_dir.path())
        .unwrap()
        .display()
        .to_string();

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
        .map(|w| {
            (
                w["path"].as_str().unwrap().to_string(),
                w["status"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(by_path[&alive], "linked");
    assert_eq!(by_path[&gone], "missing_path");

    let summary2 = run_json(
        &mut bin("repo-link", &dir),
        &[
            "worktree",
            "reconcile",
            "--workspace",
            &workspace,
            "--prune",
        ],
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
        &[
            "task",
            "create",
            "--workspace",
            &workspace,
            "--title",
            "original",
        ],
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
        &[
            "task",
            "create",
            "--workspace",
            &workspace,
            "--title",
            "v1 title",
        ],
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
        &[
            "task",
            "create",
            "--workspace",
            &workspace,
            "--title",
            "real task",
        ],
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

    let ok_row = rows
        .iter()
        .find(|r| r["task_id"] == valid_id)
        .expect("valid id row");
    assert_eq!(ok_row["ok"], true);

    let err_row = rows
        .iter()
        .find(|r| r["task_id"] == invalid_id)
        .expect("invalid id row");
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
    let canonical_token_file = token_file
        .canonicalize()
        .unwrap_or_else(|_| token_file.clone());
    assert_eq!(result["file"], canonical_token_file.display().to_string());
    assert_eq!(result["mode"], "0600");

    let mode = std::fs::metadata(&token_file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(
        std::fs::read_to_string(&token_file).unwrap().trim(),
        "abc123"
    );

    // Loosen permissions and assert any sync command rejects it.
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut sync_cmd = bin("rl", &dir);
    sync_cmd.env("REPO_LINK_GITHUB_TOKEN_FILE", &token_file);
    sync_cmd.env_remove("REPO_LINK_GITHUB_TOKEN");
    sync_cmd.env_remove("GITHUB_TOKEN");
    let output = sync_cmd
        .args([
            "sync",
            "push",
            "--task",
            "00000000-0000-0000-0000-000000000000",
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("insecure permissions"),
        "expected insecure-permissions error; got: {stderr}"
    );
    assert!(
        stderr.contains("0644"),
        "expected mode in error; got: {stderr}"
    );
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
    assert!(
        stderr.to_lowercase().contains("priority"),
        "stderr: {stderr}"
    );
}

// ---------- Phase B: names + aliases + find --------------------------------

/// Helper: attach a repo with --no-link, return the binding id.
fn attach_no_link(dir: &TempDir, workspace: &str, url: &str, canonical: &str) -> String {
    run_json(
        &mut bin("repo-link", dir),
        &[
            "repo",
            "attach",
            "--workspace",
            workspace,
            "--url",
            url,
            "--canonical",
            canonical,
            "--no-link",
        ],
    )["binding"]["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn repo_show_resolves_by_name() {
    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // canonical "github.com/o/r" → derived name is "r"
    let repo_id = attach_no_link(&dir, &workspace, "git@github.com:o/r.git", "github.com/o/r");

    let shown = run_json(&mut bin("repo-link", &dir), &["repo", "show", "r"]);
    assert_eq!(shown["id"].as_str().unwrap(), repo_id);
}

#[test]
fn repo_show_resolves_by_alias() {
    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    let repo_id = attach_no_link(&dir, &workspace, "git@github.com:o/r.git", "github.com/o/r");

    // Add alias "gateway"
    run_json(
        &mut bin("repo-link", &dir),
        &[
            "repo", "alias", "add", "--repo", &repo_id, "--alias", "gateway",
        ],
    );

    let shown = run_json(&mut bin("repo-link", &dir), &["repo", "show", "gateway"]);
    assert_eq!(shown["id"].as_str().unwrap(), repo_id);
}

#[test]
fn repo_show_errors_with_candidates_on_ambiguous() {
    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    let id1 = attach_no_link(&dir, &workspace, "git@github.com:o/a.git", "github.com/o/a");
    let id2 = attach_no_link(&dir, &workspace, "git@github.com:o/b.git", "github.com/o/b");

    // Add the same alias "shared" to both bindings
    run_json(
        &mut bin("repo-link", &dir),
        &["repo", "alias", "add", "--repo", &id1, "--alias", "shared"],
    );
    run_json(
        &mut bin("repo-link", &dir),
        &["repo", "alias", "add", "--repo", &id2, "--alias", "shared"],
    );

    let output = bin("repo-link", &dir)
        .args(["repo", "show", "shared"])
        .assert()
        .failure()
        .get_output()
        .clone();

    let stderr = String::from_utf8(output.stderr).unwrap();
    let body: serde_json::Value = serde_json::from_str(&stderr)
        .unwrap_or_else(|e| panic!("stderr is not JSON ({e}): {stderr}"));
    assert_eq!(body["error"], "ambiguous");
    let candidates = body["candidates"].as_array().expect("candidates array");
    assert_eq!(candidates.len(), 2);
}

#[test]
fn repo_rename_persists() {
    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    let repo_id = attach_no_link(&dir, &workspace, "git@github.com:o/r.git", "github.com/o/r");

    // Rename to "myrep"
    let renamed = run_json(
        &mut bin("repo-link", &dir),
        &["repo", "rename", "--repo", &repo_id, "--name", "myrep"],
    );
    assert_eq!(renamed["name"], "myrep");

    // show by new name should find it
    let shown = run_json(&mut bin("repo-link", &dir), &["repo", "show", "myrep"]);
    assert_eq!(shown["id"].as_str().unwrap(), repo_id);
}

#[test]
fn repo_alias_add_dedup() {
    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    let repo_id = attach_no_link(&dir, &workspace, "git@github.com:o/r.git", "github.com/o/r");

    // Add alias twice — second add should be idempotent
    run_json(
        &mut bin("repo-link", &dir),
        &["repo", "alias", "add", "--repo", &repo_id, "--alias", "x"],
    );
    run_json(
        &mut bin("repo-link", &dir),
        &["repo", "alias", "add", "--repo", &repo_id, "--alias", "x"],
    );

    let shown = run_json(&mut bin("repo-link", &dir), &["repo", "show", &repo_id]);
    let aliases = shown["aliases"].as_array().expect("aliases array");
    assert_eq!(
        aliases.len(),
        1,
        "duplicate alias should be deduplicated; got {:?}",
        aliases
    );
}

#[test]
fn repo_find_ranks_and_marks_ambiguous() {
    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // "search" — exact name match
    attach_no_link(
        &dir,
        &workspace,
        "git@github.com:o/search.git",
        "github.com/o/search",
    );
    // "finder" — canonical substring match for query "search" (canonical contains "search" as substring via "o/search")
    attach_no_link(
        &dir,
        &workspace,
        "git@github.com:o/finder.git",
        "github.com/o/search-tools",
    );

    let result = run_json(&mut bin("repo-link", &dir), &["repo", "find", "search"]);
    assert_eq!(result["query"], "search");
    assert_eq!(result["ambiguous"], true);
    let matches = result["matches"].as_array().expect("matches array");
    assert!(matches.len() >= 2, "expected at least 2 matches");
    // First match should be the exact name match ("search")
    assert_eq!(matches[0]["binding"]["name"], "search");
}

#[test]
fn repo_alias_rm_returns_error_when_absent() {
    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    let repo_id = attach_no_link(&dir, &workspace, "git@github.com:o/r.git", "github.com/o/r");

    // Try removing an alias that was never set
    let output = bin("repo-link", &dir)
        .args([
            "repo",
            "alias",
            "rm",
            "--repo",
            &repo_id,
            "--alias",
            "nonexistent",
        ])
        .assert()
        .failure()
        .get_output()
        .clone();

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stderr.is_empty(),
        "expected an error message on stderr; got empty"
    );
}

// ---------- repo locate ---------------------------------------------------

#[test]
fn repo_locate_returns_workspace_and_binding_for_matching_path() {
    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "ws-locate", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    let repo_dir = TempDir::new().unwrap();
    init_git_repo_with_origin(repo_dir.path(), "git@github.com:o/locate.git");
    let path = repo_dir.path().display().to_string();

    let binding_id = run_json(
        &mut bin("repo-link", &dir),
        &[
            "repo",
            "attach",
            "--workspace",
            &workspace,
            "--url",
            "git@github.com:o/locate.git",
            "--canonical",
            "github.com/o/locate",
            "--path",
            &path,
        ],
    )["binding"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let located = run_json(
        &mut bin("repo-link", &dir),
        &["repo", "locate", "--path", &path],
    );

    assert_eq!(located["canonical_url"], "github.com/o/locate");
    let matches = located["matches"].as_array().expect("matches array");
    assert_eq!(matches.len(), 1);
    let m = &matches[0];
    // Workspace is now a nested full DTO (not a bare `workspace_id` string).
    assert_eq!(m["workspace"]["id"], workspace);
    assert_eq!(m["workspace"]["name"], "ws-locate");
    assert_eq!(m["binding"]["id"], binding_id);
    assert_eq!(m["binding"]["canonical_url"], "github.com/o/locate");
}

#[test]
fn repo_locate_returns_empty_matches_for_unbound_repo() {
    let dir = TempDir::new().unwrap();
    let repo_dir = TempDir::new().unwrap();
    init_git_repo_with_origin(repo_dir.path(), "git@github.com:o/unbound.git");

    let located = run_json(
        &mut bin("repo-link", &dir),
        &["repo", "locate", "--path", &repo_dir.path().display().to_string()],
    );
    assert_eq!(located["canonical_url"], "github.com/o/unbound");
    assert!(located["matches"].as_array().unwrap().is_empty());
}

// ---------- agents docs ---------------------------------------------------

#[test]
fn agents_docs_creates_file_with_markers() {
    let dir = TempDir::new().unwrap();
    let mut cmd = bin("repo-link", &dir);
    cmd.current_dir(dir.path());
    let value = run_json(&mut cmd, &["agents", "docs"]);

    assert_eq!(value["action"], "created");
    let path = dir.path().join("AGENTS.md");
    // macOS reports tempdir paths as `/var/folders/...` but `current_dir()` in
    // the child process resolves the `/var → /private/var` symlink, so compare
    // by canonical form.
    let reported = std::path::PathBuf::from(value["file"].as_str().unwrap())
        .canonicalize()
        .unwrap();
    assert_eq!(reported, path.canonicalize().unwrap());

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("# AGENTS\n"));
    assert!(text.contains("<!-- rl:doc:start -->"));
    assert!(text.contains("<!-- rl:doc:end -->"));
    // Workflow guidance sections — the curated replacement for the
    // previous auto-generated command reference.
    assert!(text.contains("### Finding work"));
    assert!(text.contains("### Before you start: check drift"));
    assert!(text.contains("### Before you stop: sync your work"));
    // Per-repo info block. The tempdir is not a git repo, so we expect
    // the `unbound` notice.
    assert!(text.contains("## This repo"));
    assert!(text.contains("status: unbound"));
    assert_eq!(
        value["bytes_written"].as_u64().unwrap() as usize,
        text.len()
    );
}

#[test]
fn agents_docs_appends_when_existing_file_has_no_markers() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("AGENTS.md");
    std::fs::write(&path, "# AGENTS\n\nhand-written notes.\n").unwrap();

    let mut cmd = bin("repo-link", &dir);
    cmd.current_dir(dir.path());
    let value = run_json(&mut cmd, &["agents", "docs"]);

    assert_eq!(value["action"], "appended");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("# AGENTS\n\nhand-written notes.\n"));
    assert!(text.contains("## Using `rl`"));
    assert!(text.contains("<!-- rl:doc:start -->"));
}

#[test]
fn agents_docs_updates_block_on_second_run() {
    let dir = TempDir::new().unwrap();
    let mut first = bin("repo-link", &dir);
    first.current_dir(dir.path());
    let first_out = run_json(&mut first, &["agents", "docs"]);
    assert_eq!(first_out["action"], "created");
    let path = dir.path().join("AGENTS.md");
    let after_first = std::fs::read_to_string(&path).unwrap();

    let mut second = bin("repo-link", &dir);
    second.current_dir(dir.path());
    let second_out = run_json(&mut second, &["agents", "docs"]);
    assert_eq!(second_out["action"], "updated");
    let after_second = std::fs::read_to_string(&path).unwrap();
    assert_eq!(after_first, after_second);
}

#[test]
fn agents_docs_preserves_content_outside_markers_on_update() {
    // End-to-end guard: a user's hand-written preamble and epilogue, plus
    // a stale managed block between the markers, should survive an `rl
    // agents docs` regenerate byte-for-byte outside the markers. Only the
    // content between `<!-- rl:doc:start -->` and `<!-- rl:doc:end -->`
    // should change. A second run must be idempotent.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("AGENTS.md");

    let preamble = "# My Repo\n\n\
                    Custom front matter the user wrote themselves.\n\n\
                    ## Notes\n\n\
                    - keep me!\n- and me!\n\n";
    let stale_block = "<!-- rl:doc:start -->\nstale managed content\n<!-- rl:doc:end -->";
    let epilogue = "\n\n## Appendix\n\n\
                    Trailing notes that must not be clobbered.\n";

    std::fs::write(&path, format!("{preamble}{stale_block}{epilogue}")).unwrap();

    let mut cmd = bin("repo-link", &dir);
    cmd.current_dir(dir.path());
    let outcome = run_json(&mut cmd, &["agents", "docs"]);
    assert_eq!(outcome["action"], "updated");

    let text = std::fs::read_to_string(&path).unwrap();

    // Surrounding content survives byte-for-byte.
    assert!(
        text.starts_with(preamble),
        "preamble should be preserved verbatim; got: {text:?}"
    );
    assert!(
        text.ends_with(epilogue),
        "epilogue should be preserved verbatim; got: {text:?}"
    );

    // The stale managed content is gone and the freshly rendered intro is
    // in its place.
    assert!(
        !text.contains("stale managed content"),
        "stale managed content should have been replaced; got: {text:?}"
    );
    assert!(
        text.contains("`rl` (repo-link) is a local-first workspace"),
        "fresh intro should be inside the managed block; got: {text:?}"
    );

    // A second regenerate must be idempotent: identical bytes in, identical bytes out.
    let mut cmd2 = bin("repo-link", &dir);
    cmd2.current_dir(dir.path());
    let outcome2 = run_json(&mut cmd2, &["agents", "docs"]);
    assert_eq!(outcome2["action"], "updated");
    let text2 = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        text, text2,
        "second regenerate must be byte-identical (idempotent splice)"
    );
}

// ---------- daemon --------------------------------------------------------

/// All env state needed to drive `rl daemon …` against a hermetic sandbox.
/// Construct one per test: HOME is redirected so `default_launch_agent_path`
/// / `default_systemd_unit_path` write into the tempdir, REPO_LINK_LAUNCHER
/// picks the FakeLauncher mode, and REPO_LINK_RLD_PATH points the install
/// flow at a deterministic binary path (we never execute it).
struct DaemonEnv {
    home: TempDir,
    db_dir: TempDir,
    launcher_log: std::path::PathBuf,
    rld_path: std::path::PathBuf,
}

fn daemon_env() -> DaemonEnv {
    let home = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let launcher_log = db_dir.path().join("launcher.log");
    let rld_path = db_dir.path().join("rld");
    // The file's contents are irrelevant — we just need a unique path that
    // ends up baked into the rendered plist / unit so tests can assert it.
    std::fs::write(&rld_path, b"# fake rld for tests\n").unwrap();
    DaemonEnv {
        home,
        db_dir,
        launcher_log,
        rld_path,
    }
}

fn daemon_bin(env: &DaemonEnv, launcher_mode: &str) -> Command {
    let mut cmd = Command::cargo_bin("rl").expect("rl");
    cmd.env("HOME", env.home.path());
    cmd.env("REPO_LINK_DB", env.db_dir.path().join("repo-link.db"));
    cmd.env("REPO_LINK_LAUNCHER", launcher_mode);
    cmd.env("REPO_LINK_LAUNCHER_LOG", &env.launcher_log);
    cmd.env("REPO_LINK_RLD_PATH", &env.rld_path);
    // Pin XDG_CONFIG_HOME under the tempdir so the Linux systemd unit path
    // is deterministic regardless of host XDG settings (and so a macOS host
    // running the test still has a sensible XDG resolution).
    cmd.env("XDG_CONFIG_HOME", env.home.path().join(".config"));
    cmd
}

fn expected_manifest_path(env: &DaemonEnv) -> std::path::PathBuf {
    if cfg!(target_os = "macos") {
        env.home
            .path()
            .join("Library")
            .join("LaunchAgents")
            .join("com.benediktms.repo-link.plist")
    } else {
        env.home
            .path()
            .join(".config")
            .join("systemd")
            .join("user")
            .join("repo-link.service")
    }
}

#[test]
fn daemon_install_writes_manifest_with_correct_paths() {
    let env = daemon_env();
    let outcome = run_json(&mut daemon_bin(&env, "fake"), &["daemon", "install"]);

    assert_eq!(outcome["label"], "com.benediktms.repo-link");
    assert_eq!(outcome["manifest_changed"], true);
    assert_eq!(outcome["loaded"], true);

    let expected = expected_manifest_path(&env);
    let reported = std::path::PathBuf::from(outcome["manifest_path"].as_str().unwrap());
    assert_eq!(reported, expected);
    assert!(
        expected.exists(),
        "manifest file should exist at {}",
        expected.display()
    );

    let content = std::fs::read_to_string(&expected).unwrap();
    assert!(
        content.contains(env.rld_path.to_str().unwrap()),
        "manifest should reference the rld path {}: {content}",
        env.rld_path.display()
    );
}

#[test]
fn daemon_install_is_idempotent() {
    let env = daemon_env();

    let first = run_json(&mut daemon_bin(&env, "fake"), &["daemon", "install"]);
    assert_eq!(first["manifest_changed"], true);

    // Second install over identical bytes should be a no-op for the file,
    // but still re-issue the platform load sequence (bootout/bootstrap on
    // macOS, daemon-reload/enable on Linux) so we can repair drift after
    // an external `launchctl bootout`.
    let second = run_json(&mut daemon_bin(&env, "fake"), &["daemon", "install"]);
    assert_eq!(second["manifest_changed"], false);
    assert_eq!(second["loaded"], true);

    // FakeLauncher logs each argv as a JSON line. Two installs ⇒ at least
    // two recorded calls per platform, and the launcher log should be
    // non-empty.
    let log = std::fs::read_to_string(&env.launcher_log).unwrap();
    let line_count = log.lines().filter(|l| !l.is_empty()).count();
    assert!(
        line_count >= 2,
        "expected at least 2 launcher invocations across 2 installs, got {line_count}:\n{log}"
    );
}

#[test]
fn daemon_install_then_status_is_loaded_no_tick() {
    let env = daemon_env();
    let _ = run_json(&mut daemon_bin(&env, "fake"), &["daemon", "install"]);

    let status = run_json(&mut daemon_bin(&env, "fake"), &["daemon", "status"]);
    assert_eq!(status["unit_loaded"], true);
    assert!(
        status["last_tick"].is_null(),
        "last_tick should be null until the daemon writes its first heartbeat"
    );
    assert_eq!(status["wedged"], false);
    assert_eq!(status["label"], "com.benediktms.repo-link");
}

#[test]
fn daemon_status_reads_last_tick_when_present() {
    let env = daemon_env();
    let last_tick = serde_json::json!({
        "tick_at": chrono::Utc::now(),
        "interval_secs": 60,
        "report": {
            "workspaces": 1, "repos_checked": 0, "worktrees_checked": 0,
            "marked_missing": 0, "pruned": 0, "pushed": 0, "push_failures": []
        }
    });
    std::fs::write(
        env.db_dir.path().join("last_tick.json"),
        serde_json::to_string_pretty(&last_tick).unwrap(),
    )
    .unwrap();

    // fake_not_found ⇒ probe returns NotFound ⇒ unit_loaded=false. The
    // heartbeat file should still parse and wedged should be false.
    let status = run_json(
        &mut daemon_bin(&env, "fake_not_found"),
        &["daemon", "status"],
    );
    assert_eq!(status["unit_loaded"], false);
    assert_eq!(status["last_tick"]["interval_secs"], 60);
    assert_eq!(status["wedged"], false);
}

#[test]
fn daemon_status_flags_wedged_when_stale() {
    let env = daemon_env();
    let stale = chrono::Utc::now() - chrono::Duration::seconds(3600);
    let last_tick = serde_json::json!({
        "tick_at": stale,
        "interval_secs": 60,
        "report": {
            "workspaces": 1, "repos_checked": 0, "worktrees_checked": 0,
            "marked_missing": 0, "pruned": 0, "pushed": 0, "push_failures": []
        }
    });
    std::fs::write(
        env.db_dir.path().join("last_tick.json"),
        serde_json::to_string_pretty(&last_tick).unwrap(),
    )
    .unwrap();

    // wedged is gated on unit_loaded=true (see is_wedged contract): a
    // daemon that's offline is "stopped", not "wedged". Use fake mode so
    // the probe says it's loaded.
    let status = run_json(&mut daemon_bin(&env, "fake"), &["daemon", "status"]);
    assert_eq!(status["unit_loaded"], true);
    assert_eq!(status["wedged"], true);
}

#[test]
fn daemon_uninstall_is_idempotent() {
    let env = daemon_env();

    // First uninstall on a fresh state: nothing to remove, nothing to unload.
    let first = run_json(
        &mut daemon_bin(&env, "fake_not_found"),
        &["daemon", "uninstall"],
    );
    assert_eq!(first["manifest_existed"], false);
    assert_eq!(first["was_loaded"], false);

    // Second uninstall must also succeed.
    let second = run_json(
        &mut daemon_bin(&env, "fake_not_found"),
        &["daemon", "uninstall"],
    );
    assert_eq!(second["manifest_existed"], false);
    assert_eq!(second["was_loaded"], false);
}

#[test]
fn task_edit_updates_in_place_and_writes_snapshot_then_rolls_back() {
    // End-to-end contract for `rl task edit`:
    //   create v1 → edit (v2 with source=local_edit, title/body changed,
    //   omitted priority preserved) → snapshots history shows both rows
    //   → rollback to v1 restores the original values.
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();

    let created = run_json(
        &mut bin("repo-link", &dir),
        &[
            "task",
            "create",
            "--workspace",
            &workspace,
            "--title",
            "original title",
            "--body",
            "original body",
            "--priority",
            "p2",
        ],
    );
    let task_id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["title"], "original title");
    assert_eq!(created["priority"], "p2");

    let edited = run_json(
        &mut bin("repo-link", &dir),
        &[
            "task",
            "edit",
            &task_id,
            "--title",
            "revised title",
            "--body",
            "revised body",
        ],
    );
    assert_eq!(edited["id"], task_id);
    assert_eq!(edited["title"], "revised title");
    assert_eq!(edited["body"], "revised body");
    // Priority was not supplied, so it must be untouched.
    assert_eq!(edited["priority"], "p2");
    // Local-only task → no remote baseline → reconcile keeps sync_state
    // at local_only. (DirtyLocal is unreachable here on purpose; that
    // path is exercised by sync-side tests once a baseline exists.)
    assert_eq!(edited["sync_state"], "local_only");

    let snaps = run_json(
        &mut bin("repo-link", &dir),
        &["task", "snapshots", &task_id],
    );
    let rows = snaps.as_array().unwrap();
    assert_eq!(rows.len(), 2, "expected two snapshot rows, got {rows:?}");
    assert_eq!(rows[0]["version"], 1);
    assert_eq!(rows[0]["title"], "original title");
    assert_eq!(rows[1]["version"], 2);
    assert_eq!(rows[1]["title"], "revised title");
    assert_eq!(rows[1]["source"], "local_edit");

    let rolled_back = run_json(
        &mut bin("repo-link", &dir),
        &["task", "rollback", &task_id, "--to-version", "1"],
    );
    assert_eq!(rolled_back["title"], "original title");
    assert_eq!(rolled_back["body"], "original body");
}

#[test]
fn task_edit_rejects_empty_flag_set() {
    // Contract: at least one of --title/--body/--priority/--assignee must
    // be supplied. The CLI rejects the empty case at the dispatch boundary
    // even though TaskService::update would otherwise accept it.
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();
    let created = run_json(
        &mut bin("repo-link", &dir),
        &[
            "task",
            "create",
            "--workspace",
            &workspace,
            "--title",
            "x",
        ],
    );
    let task_id = created["id"].as_str().unwrap().to_string();

    let output = bin("repo-link", &dir)
        .args(["task", "edit", &task_id])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert!(
        stderr.contains("requires at least one"),
        "expected 'requires at least one' in stderr, got: {stderr}"
    );
}

#[test]
fn task_edit_unknown_id_exits_nonzero_with_readable_error() {
    // Contract: editing a task that doesn't exist must fail loudly with a
    // human-readable message — not swallow the error or panic. The actual
    // error string today is `not found: task <uuid>` (propagated from
    // `TaskRepository::get` via `?` and printed by anyhow); this test pins
    // the substring so a refactor of the error path can't silently regress
    // into something less informative.
    let dir = TempDir::new().unwrap();
    let output = bin("repo-link", &dir)
        .args([
            "task",
            "edit",
            "00000000-0000-0000-0000-000000000000",
            "--title",
            "doesn't matter",
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert!(
        stderr.to_lowercase().contains("not found"),
        "expected a 'not found' error, got: {stderr}"
    );
}

// ---- Snapshot coverage audit (GitHub #34 + #35) ---------------------------
//
// Every successful mutation of a task must append a row to `task_snapshots`,
// otherwise `rl task rollback --to-version N` has silent holes. The audit
// (per ticket #35) verifies this at the CLI layer for each lifecycle verb;
// the per-status reconcile / dirty-state sharp edges (e.g. archiving a
// Synced task → DirtyLocal) are already covered at the domain layer in
// `crates/domain-task/src/lib.rs` and aren't duplicated here.
//
// All seven lifecycle verbs route through `application-task::TaskService::
// transition`, which calls `repo.save(&t, SnapshotSource::LocalEdit)` after
// the domain method succeeds — so the contract these tests assert is
// "successful verb call → exactly one new snapshot row with source =
// local_edit and the expected post-mutation status/sync_state".

/// Create a fresh workspace + open task in a temp DB. Returns the task id.
fn fresh_task(dir: &TempDir, title: &str) -> String {
    let ws = run_json(
        &mut bin("repo-link", dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();
    let task = run_json(
        &mut bin("repo-link", dir),
        &[
            "task",
            "create",
            "--workspace",
            &workspace,
            "--title",
            title,
        ],
    );
    task["id"].as_str().unwrap().to_string()
}

/// Run a single CLI verb against a known task and assert it succeeds.
/// Returns nothing — call `task snapshots <id>` separately to inspect state.
fn run_verb(dir: &TempDir, args: &[&str]) {
    bin("repo-link", dir).args(args).assert().success();
}

#[test]
fn task_create_writes_snapshot_with_source_created() {
    // Deliverable for #34: v1 of a freshly-created task carries
    // `source = "created"`, not the misleading `local_edit` that every
    // creation used to claim. This pins the new contract so a future
    // refactor of `TaskService::create` can't silently regress.
    let dir = TempDir::new().unwrap();
    let task_id = fresh_task(&dir, "fresh");
    let snaps = run_json(
        &mut bin("repo-link", &dir),
        &["task", "snapshots", &task_id],
    );
    let rows = snaps.as_array().unwrap();
    assert_eq!(rows.len(), 1, "creation writes exactly one snapshot");
    assert_eq!(rows[0]["version"], 1);
    assert_eq!(rows[0]["source"], "created");
    assert_eq!(rows[0]["status"], "open");
    assert_eq!(rows[0]["sync_state"], "local_only");
}

#[test]
fn task_start_writes_local_edit_snapshot_with_in_progress_status() {
    let dir = TempDir::new().unwrap();
    let id = fresh_task(&dir, "t");
    run_verb(&dir, &["task", "start", &id]);
    let rows = run_json(&mut bin("repo-link", &dir), &["task", "snapshots", &id])
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(rows.len(), 2);
    let v2 = &rows[1];
    assert_eq!(v2["version"], 2);
    assert_eq!(v2["source"], "local_edit");
    assert_eq!(v2["status"], "in_progress");
}

#[test]
fn task_complete_writes_local_edit_snapshot_with_done_status() {
    // `complete` requires InProgress, so the snapshot sequence is
    // v1=created → v2=local_edit/start → v3=local_edit/complete.
    let dir = TempDir::new().unwrap();
    let id = fresh_task(&dir, "t");
    run_verb(&dir, &["task", "start", &id]);
    run_verb(&dir, &["task", "complete", &id]);
    let rows = run_json(&mut bin("repo-link", &dir), &["task", "snapshots", &id])
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(rows.len(), 3);
    let v3 = &rows[2];
    assert_eq!(v3["version"], 3);
    assert_eq!(v3["source"], "local_edit");
    assert_eq!(v3["status"], "done");
}

#[test]
fn task_reopen_writes_local_edit_snapshot_with_open_status() {
    // `reopen` requires Done, so this walks Open → InProgress → Done → Open.
    // The reopen target is `Open`, not `InProgress` — pinning that so a
    // future "reopen restores prior status" change can't pass silently.
    let dir = TempDir::new().unwrap();
    let id = fresh_task(&dir, "t");
    run_verb(&dir, &["task", "start", &id]);
    run_verb(&dir, &["task", "complete", &id]);
    run_verb(&dir, &["task", "reopen", &id]);
    let rows = run_json(&mut bin("repo-link", &dir), &["task", "snapshots", &id])
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(rows.len(), 4);
    let v4 = &rows[3];
    assert_eq!(v4["version"], 4);
    assert_eq!(v4["source"], "local_edit");
    assert_eq!(v4["status"], "open");
}

#[test]
fn task_block_writes_local_edit_snapshot_with_blocked_status() {
    let dir = TempDir::new().unwrap();
    let id = fresh_task(&dir, "t");
    run_verb(&dir, &["task", "block", &id]);
    let rows = run_json(&mut bin("repo-link", &dir), &["task", "snapshots", &id])
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(rows.len(), 2);
    let v2 = &rows[1];
    assert_eq!(v2["source"], "local_edit");
    assert_eq!(v2["status"], "blocked");
}

#[test]
fn task_unblock_writes_local_edit_snapshot_with_open_status() {
    // Walks Open → Blocked → Open. Same "back to Open, not prior status"
    // contract as `reopen`.
    let dir = TempDir::new().unwrap();
    let id = fresh_task(&dir, "t");
    run_verb(&dir, &["task", "block", &id]);
    run_verb(&dir, &["task", "unblock", &id]);
    let rows = run_json(&mut bin("repo-link", &dir), &["task", "snapshots", &id])
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(rows.len(), 3);
    let v3 = &rows[2];
    assert_eq!(v3["source"], "local_edit");
    assert_eq!(v3["status"], "open");
}

#[test]
fn task_archive_writes_local_edit_snapshot_with_archived_status() {
    // Archive accepts any non-Archived status, so we go straight from Open
    // → Archived without intermediate steps. (The domain-layer test
    // `lifecycle_mutations_on_synced_remote_task_flip_to_dirty_local`
    // already covers the sync-state flip for remote-backed tasks.)
    let dir = TempDir::new().unwrap();
    let id = fresh_task(&dir, "t");
    run_verb(&dir, &["task", "archive", &id]);
    let rows = run_json(&mut bin("repo-link", &dir), &["task", "snapshots", &id])
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(rows.len(), 2);
    let v2 = &rows[1];
    assert_eq!(v2["source"], "local_edit");
    assert_eq!(v2["status"], "archived");
}

#[test]
fn task_stage_writes_local_edit_snapshot_with_staged_sync_state() {
    // `stage` is the outlier — it mutates `sync_state`, not `status`.
    // The domain method deliberately skips `reconcile_dirty_against_baseline`
    // (lifecycle and sync are orthogonal), but `transition` still calls
    // `save()`, so a snapshot row appears.
    let dir = TempDir::new().unwrap();
    let id = fresh_task(&dir, "t");
    run_verb(&dir, &["task", "stage", &id]);
    let rows = run_json(&mut bin("repo-link", &dir), &["task", "snapshots", &id])
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(rows.len(), 2);
    let v2 = &rows[1];
    assert_eq!(v2["source"], "local_edit");
    assert_eq!(v2["sync_state"], "staged");
    // Status is unchanged — lifecycle and sync are orthogonal.
    assert_eq!(v2["status"], "open");
}

#[test]
fn task_rollback_restores_status_across_lifecycle_transition() {
    // The rollback contract this PR is most about: rolling back across a
    // status transition correctly restores the prior status. Without
    // complete snapshot coverage at every verb, a rollback could silently
    // skip a transition and leave the task in an inconsistent state.
    let dir = TempDir::new().unwrap();
    let id = fresh_task(&dir, "lifecycle rollback");
    run_verb(&dir, &["task", "start", &id]);
    let after_start = run_json(&mut bin("repo-link", &dir), &["task", "show", &id]);
    assert_eq!(after_start["status"], "in_progress");

    let rolled_back = run_json(
        &mut bin("repo-link", &dir),
        &["task", "rollback", &id, "--to-version", "1"],
    );
    assert_eq!(rolled_back["status"], "open");

    // Confirm the rollback itself appended a snapshot (source = rollback),
    // so the history is v1=created, v2=local_edit/start, v3=rollback/open.
    let rows = run_json(&mut bin("repo-link", &dir), &["task", "snapshots", &id])
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["source"], "created");
    assert_eq!(rows[1]["source"], "local_edit");
    assert_eq!(rows[2]["source"], "rollback");
    assert_eq!(rows[2]["status"], "open");
}
