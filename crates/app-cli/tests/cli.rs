use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
fn workspace_edit_updates_mutable_fields_by_name() {
    let dir = TempDir::new().unwrap();
    let created = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "scratch", "--local-only"],
    );
    let id = created["id"].as_str().expect("id").to_string();
    let created_at = created["created_at"].clone();
    let updated_at = created["updated_at"].clone();

    let edited = run_json(
        &mut bin("repo-link", &dir),
        &[
            "workspace",
            "edit",
            "scratch",
            "--name",
            "renamed",
            "--description",
            "new description",
        ],
    );

    assert_eq!(edited["id"], id);
    assert_eq!(edited["name"], "renamed");
    assert_eq!(edited["description"], "new description");
    assert_eq!(edited["created_at"], created_at);
    assert_ne!(edited["updated_at"], updated_at);

    let shown = run_json(&mut bin("repo-link", &dir), &["workspace", "show", &id]);
    assert_eq!(shown["name"], "renamed");
    assert_eq!(shown["description"], "new description");
    assert_eq!(shown["created_at"], created_at);
    assert_eq!(shown["updated_at"], edited["updated_at"]);
}

#[test]
fn workspace_edit_falls_back_for_uuid_shaped_name() {
    let dir = TempDir::new().unwrap();
    let name = "00000000-0000-0000-0000-000000000000";
    let created = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", name, "--local-only"],
    );
    let id = created["id"].as_str().expect("id").to_string();
    let created_at = created["created_at"].clone();
    let updated_at = created["updated_at"].clone();

    let edited = run_json(
        &mut bin("repo-link", &dir),
        &[
            "workspace",
            "edit",
            name,
            "--description",
            "uuid-looking name still resolves",
        ],
    );

    assert_eq!(edited["id"], id);
    assert_eq!(edited["name"], name);
    assert_eq!(edited["description"], "uuid-looking name still resolves");
    assert_eq!(edited["created_at"], created_at);
    assert_ne!(edited["updated_at"], updated_at);

    let shown = run_json(&mut bin("repo-link", &dir), &["workspace", "show", &id]);
    assert_eq!(shown["updated_at"], edited["updated_at"]);
}

#[test]
fn workspace_edit_rejects_empty_flag_set() {
    let dir = TempDir::new().unwrap();
    let created = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "scratch", "--local-only"],
    );
    let id = created["id"].as_str().expect("id");

    let output = bin("repo-link", &dir)
        .args(["workspace", "edit", id])
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
fn workspace_edit_rejects_duplicate_name() {
    let dir = TempDir::new().unwrap();
    run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "a", "--local-only"],
    );
    let b = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "b", "--local-only"],
    );
    let b_id = b["id"].as_str().expect("id");

    let output = bin("repo-link", &dir)
        .args(["workspace", "edit", b_id, "--name", "a"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert!(
        stderr.contains("workspace name already in use: a"),
        "expected friendly duplicate-name error, got: {stderr}"
    );

    let shown = run_json(&mut bin("repo-link", &dir), &["workspace", "show", b_id]);
    assert_eq!(shown["name"], "b");
}

/// RFC 0002 §4 (#121): a workspace filing default must be one of THAT
/// workspace's own bindings. The repo-handle resolver searches all workspaces,
/// so `set-filing-repo` rejects a binding owned by a different workspace; a
/// same-workspace binding is accepted and recorded.
#[test]
fn set_filing_repo_rejects_a_binding_from_another_workspace() {
    let dir = TempDir::new().unwrap();
    let ws_a = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "a", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let ws_b = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "b", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // A binding owned by workspace B must not become workspace A's default.
    let repo_b = attach_no_link(&dir, &ws_b, "git@github.com:o/b.git", "github.com/o/b");
    let output = bin("repo-link", &dir)
        .args(["workspace", "set-filing-repo", &ws_a, "--repo", &repo_b])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("belongs to workspace"),
        "expected a cross-workspace rejection, got: {stderr}"
    );

    // A binding owned by workspace A is accepted and recorded.
    let repo_a = attach_no_link(&dir, &ws_a, "git@github.com:o/a.git", "github.com/o/a");
    let dto = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "set-filing-repo", &ws_a, "--repo", &repo_a],
    );
    // RFC 0005 §D4: the filing axis lives in ORIGIN id space — the workspace
    // default is recorded as the shared origin id, NOT the per-workspace
    // instance id the handle resolved to. (The promote path reads
    // `workspace.filing_repo_id` as an origin id; storing the instance id here
    // would resolve to a nonexistent origin for any freshly-attached repo,
    // where instance.id != origin.id.)
    let origin_a = origin_id_of(&dir, &repo_a);
    assert_ne!(
        origin_a, repo_a,
        "instance and origin ids differ for a fresh attach"
    );
    assert_eq!(dto["filing_repo_id"], origin_a);
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
    assert_eq!(task["is_open"], true);
    assert_eq!(task["state_reason"], serde_json::Value::Null);
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

/// RFC 0002 #122 (option a): `task create --filing-repo` resolves the handle
/// (to validate it / surface ambiguity like `--repo`) then REJECTS with a
/// deferral error — `task create` only mints a local draft and has no filing
/// transition to consume the override. The flag must never be a silent no-op.
#[test]
fn task_create_filing_repo_override_is_deferred() {
    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    // A resolvable filing-repo handle (so we hit the deferral, not a not-found).
    let repo_id = attach_no_link(&dir, &workspace, "git@github.com:o/r.git", "github.com/o/r");

    let output = bin("repo-link", &dir)
        .args([
            "task",
            "create",
            "--workspace",
            &workspace,
            "--title",
            "t",
            "--filing-repo",
            &repo_id,
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("not yet consumed by `task create`"),
        "expected the --filing-repo deferral error, got: {stderr}"
    );
    // The rejected create must not have persisted a task.
    let listed = run_json(&mut bin("repo-link", &dir), &["task", "list"]);
    assert!(
        listed.as_array().unwrap().is_empty(),
        "a rejected create must not persist a task"
    );
}

/// RFC 0002 #122 / D5: `task show` overlays an additive `filing_repo` block
/// (null for an unpromoted task) WITHOUT leaking the internal `filing_repo_id`
/// onto the task surface, and leaves the base TaskDto + list shapes unchanged.
#[test]
fn task_show_surfaces_filing_repo_without_leaking_filing_repo_id() {
    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let task = run_json(
        &mut bin("repo-link", &dir),
        &["task", "create", "--workspace", &workspace, "--title", "t"],
    );
    let task_id = task["id"].as_str().unwrap().to_string();

    let shown = run_json(&mut bin("repo-link", &dir), &["task", "show", &task_id]);
    // Additive key present and null for an unpromoted task.
    assert!(
        shown.as_object().unwrap().contains_key("filing_repo"),
        "task show must include the additive filing_repo key"
    );
    assert!(
        shown["filing_repo"].is_null(),
        "an unpromoted task has no recorded filing repo"
    );
    // D5: the internal filing_repo_id must NEVER appear on the task surface.
    assert!(
        !shown.as_object().unwrap().contains_key("filing_repo_id"),
        "filing_repo_id must never leak onto the task surface (D5)"
    );
    // Base TaskDto shape intact.
    assert_eq!(shown["id"], task_id);
    assert_eq!(shown["is_open"], true);
    assert_eq!(shown["state_reason"], Value::Null);
    assert_eq!(shown["repo_id"], Value::Null);
    // list keeps the byte-identical TaskDto shape — no show-only overlay.
    let listed = run_json(&mut bin("repo-link", &dir), &["task", "list"]);
    assert!(
        !listed[0].as_object().unwrap().contains_key("filing_repo"),
        "list must not carry the show-only filing_repo overlay"
    );
}

#[test]
fn task_comment_creates_pending_and_surfaces_in_unsynced() {
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
        ],
    );
    let task_id = task["id"].as_str().unwrap().to_string();

    // Adding a comment returns the task with one pending (remote_id null)
    // comment, and must not dirty the task's snapshot axis.
    let after = run_json(
        &mut bin("repo-link", &dir),
        &["task", "comment", &task_id, "looks good"],
    );
    let comments = after["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["body"], "looks good");
    assert!(comments[0]["remote_id"].is_null());
    assert_eq!(after["sync_state"], "local_only");

    // It persists across reads.
    let shown = run_json(&mut bin("repo-link", &dir), &["task", "show", &task_id]);
    assert_eq!(shown["comments"].as_array().unwrap().len(), 1);

    // query unsynced reports the pending-comment count (rows key by UUID, so
    // assert on the single workspace task rather than the composite id).
    let unsynced = run_json(
        &mut bin("repo-link", &dir),
        &["query", "unsynced", "--workspace", &workspace],
    );
    let rows = unsynced.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["pending_comments"], 1);

    // Empty body is rejected.
    bin("repo-link", &dir)
        .args(["task", "comment", &task_id, "   "])
        .assert()
        .failure();
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
        assert_eq!(row["task"]["is_open"], true);
    }
}

/// `rl task relate` (rpl-7oz) must wrap its return in a `{ok, task}` /
/// `{ok: false, error}` envelope so a caller can tell whether the edge was
/// actually added. The pre-fix shape was a bare `TaskDto` (success) or
/// `Error: ...` on stderr (failure) — indistinguishable to a JSON-pipe
/// consumer, which forced a follow-up `rl task show` to verify.
#[test]
fn task_relate_emits_envelope_on_success_and_failure() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();

    let a = run_json(
        &mut bin("repo-link", &dir),
        &["task", "create", "--workspace", &workspace, "--title", "a"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = run_json(
        &mut bin("repo-link", &dir),
        &["task", "create", "--workspace", &workspace, "--title", "b"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Success: a blocked_by b. The envelope carries `ok: true` plus the
    // freshly-mutated task; the relations list reflects the new edge.
    let out = bin("repo-link", &dir)
        .args(["task", "relate", &a, "--kind", "blocked_by", "--other", &b])
        .assert()
        .success()
        .get_output()
        .clone();
    let v: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout not JSON ({e}): {:?}", out.stdout));
    assert_eq!(v["ok"], true, "success envelope must set ok: true");
    assert_eq!(v["task"]["id"], a);
    let rels = v["task"]["relations"].as_array().expect("relations array");
    assert!(
        rels.iter()
            .any(|r| r["kind"] == "blocked_by" && r["other"] == b),
        "expected the new blocked_by edge in relations[]: {rels:?}"
    );

    // Failure: self-relation. The envelope prints to stdout, the same
    // string lands on stderr (mirroring render::sync's split), and the
    // process exits non-zero so a `set -e` shell sees the failure.
    let out = bin("repo-link", &dir)
        .args(["task", "relate", &a, "--kind", "related_to", "--other", &a])
        .assert()
        .failure()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let stderr = String::from_utf8(out.stderr).expect("utf-8 stderr");
    let v: Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("stdout not JSON ({e}): {stdout}"));
    assert_eq!(v["ok"], false, "failure envelope must set ok: false");
    assert_eq!(v["error"], "a task cannot be related to itself");
    // Pin stderr to the exact single line the command emits (`eprintln!("error: {msg}")`
    // followed by `std::process::exit(1)`). A loose `contains` check would
    // hide duplicate-line regressions if the bin shim's Termination impl
    // ever started re-emitting the error.
    assert_eq!(stderr, "error: a task cannot be related to itself\n");

    // Failure: cycle. The earlier success case added `a blocked_by b`.
    // Now add `b blocked_by a` — which would close a deadlock loop and
    // the service returns RelationCycle.
    let out = bin("repo-link", &dir)
        .args(["task", "relate", &b, "--kind", "blocked_by", "--other", &a])
        .assert()
        .failure()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let v: Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("stdout not JSON ({e}): {stdout}"));
    assert_eq!(v["ok"], false);
    let err = v["error"].as_str().expect("error string");
    assert!(
        err.contains("would create a cycle"),
        "expected cycle message; got: {err}"
    );
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

    // Complete the task — a real lifecycle transition produces a second
    // snapshot (`start` on an open task is a no-op and appends nothing).
    bin("repo-link", &dir)
        .args(["task", "complete", &task_id])
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
    // Point the GH API at a closed port so `gh auth`'s best-effort `/user`
    // call fails fast (connection refused). The token still persists; the
    // login simply isn't cached, matching offline / bad-token UX.
    cmd.env("REPO_LINK_GITHUB_API_BASE_URL", "http://127.0.0.1:1");
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

// `sync import` validates the URL and resolves the repo binding *before* any
// network call, so these paths are testable with a dummy token and no mock.

#[test]
fn sync_import_rejects_non_issue_url() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("rl", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let ws_id = ws["id"].as_str().unwrap();

    let mut cmd = bin("rl", &dir);
    cmd.env("REPO_LINK_GITHUB_TOKEN", "dummy");
    let output = cmd
        .args(["sync", "import", "not-a-url", "--workspace", ws_id])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("not a github issue url"),
        "expected url-parse error; got: {stderr}"
    );
}

#[test]
fn sync_import_errors_when_repo_unbound() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("rl", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let ws_id = ws["id"].as_str().unwrap();

    let mut cmd = bin("rl", &dir);
    cmd.env("REPO_LINK_GITHUB_TOKEN", "dummy");
    let output = cmd
        .args([
            "sync",
            "import",
            "https://github.com/o/r/issues/1",
            "--workspace",
            ws_id,
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("no repo binding for github.com/o/r"),
        "expected unbound-repo error; got: {stderr}"
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

/// Resolve a repo handle (UUID / name / alias) to its shared ORIGIN id
/// (RFC 0005). The filing axis stores origin ids, so filing-default
/// assertions compare against this — not the per-workspace instance id
/// `attach_no_link` returns.
fn origin_id_of(dir: &TempDir, handle: &str) -> String {
    run_json(&mut bin("repo-link", dir), &["repo", "show", handle])["origin_id"]
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
        &[
            "repo",
            "locate",
            "--path",
            &repo_dir.path().display().to_string(),
        ],
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

/// Regression for #132: `install` must `enable` the label BEFORE `bootstrap`.
/// A unit left *disabled* (e.g. by an external `launchctl bootout`/disable)
/// makes `bootstrap` fail with errno 5, so without enabling first neither
/// `rl daemon install` nor `just install` can recover it. macOS-only: the
/// Linux systemd sequence has no `bootstrap` step.
#[test]
#[cfg(target_os = "macos")]
fn daemon_install_enables_before_bootstrap() {
    let env = daemon_env();
    run_json(&mut daemon_bin(&env, "fake"), &["daemon", "install"]);

    let log = std::fs::read_to_string(&env.launcher_log).unwrap();
    let lines: Vec<&str> = log.lines().filter(|l| !l.is_empty()).collect();
    let enable_idx = lines
        .iter()
        .position(|l| l.contains("\"enable\""))
        .expect("install issues launchctl enable");
    let bootstrap_idx = lines
        .iter()
        .position(|l| l.contains("\"bootstrap\""))
        .expect("install issues launchctl bootstrap");
    assert!(
        enable_idx < bootstrap_idx,
        "enable must precede bootstrap so a disabled unit can be re-bootstrapped; log:\n{log}"
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
        &["task", "create", "--workspace", &workspace, "--title", "x"],
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
    assert_eq!(rows[0]["lifecycle"], "open");
    assert_eq!(rows[0]["sync_state"], "local_only");
}

#[test]
fn task_start_on_open_task_is_a_noop() {
    // RFC 0004: `start` no longer has an `InProgress` target — it just asserts
    // the open state. On an already-open task it is a true no-op: it appends NO
    // snapshot and does not flip sync state, so a redundant `rl task start`
    // never enqueues outbound churn.
    let dir = TempDir::new().unwrap();
    let id = fresh_task(&dir, "t");
    run_verb(&dir, &["task", "start", &id]);
    let rows = run_json(&mut bin("repo-link", &dir), &["task", "snapshots", &id])
        .as_array()
        .cloned()
        .unwrap();
    // Only the v1 `created` snapshot — `start` appended nothing.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["version"], 1);
    assert_eq!(rows[0]["lifecycle"], "open");
}

#[test]
fn task_complete_writes_local_edit_snapshot_with_completed_status() {
    // `complete` closes the task as `Completed` (RFC 0004). `start` on an open
    // task is a no-op (appends nothing), so the sequence is just
    // v1=created → v2=local_edit/complete.
    let dir = TempDir::new().unwrap();
    let id = fresh_task(&dir, "t");
    run_verb(&dir, &["task", "complete", &id]);
    let rows = run_json(&mut bin("repo-link", &dir), &["task", "snapshots", &id])
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(rows.len(), 2);
    let v2 = &rows[1];
    assert_eq!(v2["version"], 2);
    assert_eq!(v2["source"], "local_edit");
    assert_eq!(v2["lifecycle"], "completed");
}

#[test]
fn task_reopen_writes_local_edit_snapshot_with_reopened_status() {
    // `reopen` transitions a closed task back to open with the distinct
    // `Reopened` marker (RFC 0004). Walks open → completed → reopened (`start`
    // is a no-op and is omitted).
    let dir = TempDir::new().unwrap();
    let id = fresh_task(&dir, "t");
    run_verb(&dir, &["task", "complete", &id]);
    run_verb(&dir, &["task", "reopen", &id]);
    let rows = run_json(&mut bin("repo-link", &dir), &["task", "snapshots", &id])
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(rows.len(), 3);
    let v3 = &rows[2];
    assert_eq!(v3["version"], 3);
    assert_eq!(v3["source"], "local_edit");
    assert_eq!(v3["lifecycle"], "reopened");
}

#[test]
fn task_archive_writes_local_edit_snapshot_with_not_planned_status() {
    // RFC 0004: archive folds into the closed/`not_planned` lifecycle. We go
    // straight from open → not_planned without intermediate steps.
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
    assert_eq!(v2["lifecycle"], "not_planned");
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
    // Lifecycle is unchanged — lifecycle and sync are orthogonal.
    assert_eq!(v2["lifecycle"], "open");
}

#[test]
fn task_rollback_restores_status_across_lifecycle_transition() {
    // The rollback contract this PR is most about: rolling back across a
    // lifecycle transition correctly restores the prior state. Without
    // complete snapshot coverage at every verb, a rollback could silently
    // skip a transition and leave the task in an inconsistent state.
    let dir = TempDir::new().unwrap();
    let id = fresh_task(&dir, "lifecycle rollback");
    run_verb(&dir, &["task", "complete", &id]);
    let after_complete = run_json(&mut bin("repo-link", &dir), &["task", "show", &id]);
    assert_eq!(after_complete["is_open"], false);
    assert_eq!(after_complete["state_reason"], "completed");

    let rolled_back = run_json(
        &mut bin("repo-link", &dir),
        &["task", "rollback", &id, "--to-version", "1"],
    );
    assert_eq!(rolled_back["is_open"], true);
    assert_eq!(rolled_back["state_reason"], serde_json::Value::Null);

    // Confirm the rollback itself appended a snapshot (source = rollback),
    // so the history is v1=created, v2=local_edit/complete, v3=rollback/open.
    let rows = run_json(&mut bin("repo-link", &dir), &["task", "snapshots", &id])
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["source"], "created");
    assert_eq!(rows[1]["source"], "local_edit");
    assert_eq!(rows[2]["source"], "rollback");
    assert_eq!(rows[2]["lifecycle"], "open");
}

// ---- Friendly task IDs ---------------------------------------------------
//
// These tests exercise the user-visible surface added in the
// prefix-hash work: composite display IDs, bare-hash + composite +
// UUID resolution, prefix-mismatch errors, explicit `--prefix` on
// `repo attach`, and the `repo set-prefix` override.

#[test]
fn task_id_renders_as_prefix_hash_when_repo_is_bound() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();

    let outcome = run_json(
        &mut bin("repo-link", &dir),
        &[
            "repo",
            "attach",
            "--workspace",
            &workspace,
            "--url",
            "git@github.com:org/app-payments.git",
            "--canonical",
            "github.com/org/app-payments",
            "--no-link",
        ],
    );
    let repo_id = outcome["binding"]["id"].as_str().unwrap().to_string();
    // `app-payments` → `app` stripped as noise → single-word `payments`
    // → `p` + first two consonants → `pym`.
    assert_eq!(outcome["binding"]["prefix"], "pym");

    let task = run_json(
        &mut bin("repo-link", &dir),
        &[
            "task",
            "create",
            "--workspace",
            &workspace,
            "--repo",
            &repo_id,
            "--title",
            "wire up Stripe webhook",
        ],
    );

    let id = task["id"].as_str().unwrap();
    let (prefix, hash) = id.split_once('-').expect("composite id");
    assert_eq!(prefix, "pym", "prefix should match the binding");
    assert!(
        hash.chars().all(|c| matches!(c, 'a'..='z' | '2'..='7')),
        "hash {hash:?} should be lowercase base32"
    );
    assert!(hash.len() >= 3, "hash too short");
}

#[test]
fn task_without_repo_falls_back_to_bare_hash() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap();

    let task = run_json(
        &mut bin("repo-link", &dir),
        &[
            "task",
            "create",
            "--workspace",
            workspace,
            "--title",
            "no repo on this one",
        ],
    );
    let id = task["id"].as_str().unwrap();
    // Bare hash → no `-`.
    assert!(!id.contains('-'), "expected bare hash, got {id:?}");
    assert!(id.len() >= 3 && id.chars().all(|c| matches!(c, 'a'..='z' | '2'..='7')));
}

#[test]
fn task_resolves_by_composite_and_bare_hash() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();
    let outcome = run_json(
        &mut bin("repo-link", &dir),
        &[
            "repo",
            "attach",
            "--workspace",
            &workspace,
            "--url",
            "git@github.com:org/repo-link.git",
            "--canonical",
            "github.com/org/repo-link",
            "--no-link",
        ],
    );
    let repo_id = outcome["binding"]["id"].as_str().unwrap().to_string();
    let prefix = outcome["binding"]["prefix"].as_str().unwrap().to_string();

    let task = run_json(
        &mut bin("repo-link", &dir),
        &[
            "task",
            "create",
            "--workspace",
            &workspace,
            "--repo",
            &repo_id,
            "--title",
            "resolver smoke test",
        ],
    );
    let composite = task["id"].as_str().unwrap().to_string();
    let hash = composite.split_once('-').unwrap().1.to_string();

    // Composite resolves.
    let by_composite = run_json(&mut bin("repo-link", &dir), &["task", "show", &composite]);
    assert_eq!(by_composite["title"], "resolver smoke test");

    // Bare hash resolves.
    let by_hash = run_json(&mut bin("repo-link", &dir), &["task", "show", &hash]);
    assert_eq!(by_hash["id"], composite);

    // Wrong prefix produces a hard error mentioning the actual prefix.
    let err = bin("repo-link", &dir)
        .args(["task", "show", &format!("wrong-{hash}")])
        .assert()
        .failure();
    let stderr = String::from_utf8(err.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("prefix mismatch") && stderr.contains(&prefix),
        "expected mismatch error mentioning '{prefix}', got: {stderr}"
    );
}

#[test]
fn repo_attach_explicit_prefix_overrides_derived_value() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap();

    let outcome = run_json(
        &mut bin("repo-link", &dir),
        &[
            "repo",
            "attach",
            "--workspace",
            workspace,
            "--url",
            "git@github.com:org/some-thing.git",
            "--canonical",
            "github.com/org/some-thing",
            "--no-link",
            "--prefix",
            "xyz",
        ],
    );
    // Without override the derived prefix would be something else; the
    // explicit value wins.
    assert_eq!(outcome["binding"]["prefix"], "xyz");
}

#[test]
fn repo_set_prefix_changes_the_prefix_in_place() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();
    let outcome = run_json(
        &mut bin("repo-link", &dir),
        &[
            "repo",
            "attach",
            "--workspace",
            &workspace,
            "--url",
            "git@github.com:org/auth-service.git",
            "--canonical",
            "github.com/org/auth-service",
            "--no-link",
        ],
    );
    // `service` is a noise word → derived prefix is `ath`.
    let original_prefix = outcome["binding"]["prefix"].as_str().unwrap().to_string();
    assert_eq!(original_prefix, "ath");
    let repo_id = outcome["binding"]["id"].as_str().unwrap().to_string();

    let renamed = run_json(
        &mut bin("repo-link", &dir),
        &["repo", "set-prefix", "--repo", &repo_id, "--prefix", "auth"],
    );
    assert_eq!(renamed["prefix"], "auth");

    // Repo now resolvable by the new prefix.
    let by_new = run_json(&mut bin("repo-link", &dir), &["repo", "show", "auth"]);
    assert_eq!(by_new["id"], repo_id);
}

#[test]
fn task_show_rejects_malformed_id_with_clear_error() {
    let dir = TempDir::new().unwrap();
    run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    // Uppercase is not valid base32 → should be a "bad id" style error,
    // not a confusing "task hash not found".
    let assert = bin("repo-link", &dir)
        .args(["task", "show", "ZZZ"])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("not a task UUID, bare hash, or prefix-hash composite"),
        "expected bad-id message, got: {stderr}"
    );
}

#[test]
fn repo_attach_accepts_long_manual_prefix() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap();
    // A manual prefix longer than the 3-char derived default is allowed
    // (up to the 20-char cap).
    let outcome = run_json(
        &mut bin("repo-link", &dir),
        &[
            "repo",
            "attach",
            "--workspace",
            workspace,
            "--url",
            "git@github.com:o/thing.git",
            "--canonical",
            "github.com/o/thing",
            "--no-link",
            "--prefix",
            "mylongprefix",
        ],
    );
    assert_eq!(outcome["binding"]["prefix"], "mylongprefix");
    // And resolvable by that handle.
    let shown = run_json(
        &mut bin("repo-link", &dir),
        &["repo", "show", "mylongprefix"],
    );
    assert_eq!(shown["prefix"], "mylongprefix");
}

#[test]
fn repo_set_prefix_conflict_is_friendly() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();
    // Attach two repos with explicit, distinct prefixes.
    let a = run_json(
        &mut bin("repo-link", &dir),
        &[
            "repo",
            "attach",
            "--workspace",
            &workspace,
            "--url",
            "git@github.com:o/a.git",
            "--canonical",
            "github.com/o/a",
            "--no-link",
            "--prefix",
            "aaa",
        ],
    );
    run_json(
        &mut bin("repo-link", &dir),
        &[
            "repo",
            "attach",
            "--workspace",
            &workspace,
            "--url",
            "git@github.com:o/b.git",
            "--canonical",
            "github.com/o/b",
            "--no-link",
            "--prefix",
            "bbb",
        ],
    );
    let a_id = a["binding"]["id"].as_str().unwrap();
    // Try to rename repo A's prefix to one already owned by repo B.
    let assert = bin("repo-link", &dir)
        .args(["repo", "set-prefix", "--repo", a_id, "--prefix", "bbb"])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("already taken") && stderr.contains("bbb"),
        "expected friendly prefix-taken error, got: {stderr}"
    );
}

#[test]
fn repo_attach_collision_breaks_with_numeric_suffix() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();
    // Two repos whose names both derive to `ath` (`service` is a noise
    // word in both cases, leaving the single-word `auth` → `ath`).
    let a = run_json(
        &mut bin("repo-link", &dir),
        &[
            "repo",
            "attach",
            "--workspace",
            &workspace,
            "--url",
            "git@github.com:o1/auth-service.git",
            "--canonical",
            "github.com/o1/auth-service",
            "--no-link",
        ],
    );
    let b = run_json(
        &mut bin("repo-link", &dir),
        &[
            "repo",
            "attach",
            "--workspace",
            &workspace,
            "--url",
            "git@github.com:o2/auth-service.git",
            "--canonical",
            "github.com/o2/auth-service",
            "--no-link",
        ],
    );
    assert_eq!(a["binding"]["prefix"], "ath");
    // Second attach derives the same base but the collision-break
    // suffix kicks in.
    assert_eq!(b["binding"]["prefix"], "ath1");
}

#[test]
fn task_edit_repo_attaches_repo_and_makes_id_composite() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();
    let binding = run_json(
        &mut bin("repo-link", &dir),
        &[
            "repo",
            "attach",
            "--workspace",
            &workspace,
            "--url",
            "git@github.com:o/widget.git",
            "--canonical",
            "github.com/o/widget",
            "--no-link",
            "--prefix",
            "wid",
        ],
    );
    let repo_id = binding["binding"]["id"].as_str().unwrap().to_string();

    // Create a task with NO repo → bare-hash id, repo_id null.
    let task = run_json(
        &mut bin("repo-link", &dir),
        &[
            "task",
            "create",
            "--workspace",
            &workspace,
            "--title",
            "orphan task",
        ],
    );
    let original_id = task["id"].as_str().unwrap().to_string();
    assert!(
        !original_id.contains('-'),
        "expected bare hash, got {original_id}"
    );
    assert!(task["repo_id"].is_null());

    // Attach the repo via edit (using the -r short flag) → id becomes a
    // `wid-<hash>` composite.
    let edited = run_json(
        &mut bin("repo-link", &dir),
        &["task", "edit", &original_id, "-r", &repo_id],
    );
    assert_eq!(edited["repo_id"], repo_id);
    let new_id = edited["id"].as_str().unwrap();
    assert_eq!(
        new_id,
        format!("wid-{original_id}"),
        "id should gain the wid- prefix"
    );

    // The task now resolves by its composite id too.
    let shown = run_json(&mut bin("repo-link", &dir), &["task", "show", new_id]);
    assert_eq!(shown["title"], "orphan task");
}

#[test]
fn task_edit_with_no_flags_is_rejected() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap();
    let task = run_json(
        &mut bin("repo-link", &dir),
        &["task", "create", "--workspace", workspace, "--title", "t"],
    );
    let id = task["id"].as_str().unwrap();
    let assert = bin("repo-link", &dir)
        .args(["task", "edit", id])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("--repo"),
        "the at-least-one-flag error should now mention --repo: {stderr}"
    );
}

// ---------- Issue #47: handle resolution audit on `--repo` everywhere -------
//
// `task create/edit --repo` and `repo {rename,set-prefix,alias}` already
// resolve via `RepoBindingService::show`. The tests below cover the sites
// that were UUID-only before the audit: `repo detach`, `worktree link`,
// `worktree unlink`, `worktree prune-missing`. One ambiguous-handle test
// stands in for every site since they all share the same resolver helper.

#[test]
fn repo_detach_resolves_by_prefix() {
    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    let repo_id = attach_no_link(&dir, &workspace, "git@github.com:o/r.git", "github.com/o/r");
    let shown = run_json(&mut bin("repo-link", &dir), &["repo", "show", &repo_id]);
    let prefix = shown["prefix"].as_str().unwrap().to_string();

    // Detach by prefix — the response should echo the resolved UUID, not the
    // input handle, so callers can audit what was actually targeted.
    let detached = run_json(&mut bin("repo-link", &dir), &["repo", "detach", &prefix]);
    assert_eq!(detached["detached"].as_str().unwrap(), repo_id);

    // The binding is gone.
    bin("repo-link", &dir)
        .args(["repo", "show", &repo_id])
        .assert()
        .failure();
}

#[test]
fn worktree_link_resolves_by_prefix() {
    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let repo_id = attach_no_link(&dir, &workspace, "git@github.com:o/r.git", "github.com/o/r");
    let prefix = run_json(&mut bin("repo-link", &dir), &["repo", "show", &repo_id])["prefix"]
        .as_str()
        .unwrap()
        .to_string();

    let repo_dir = TempDir::new().unwrap();
    init_git_repo_with_origin(repo_dir.path(), "git@github.com:o/r.git");

    let linked = run_json(
        &mut bin("repo-link", &dir),
        &[
            "worktree",
            "link",
            "--repo",
            &prefix,
            "--path",
            &repo_dir.path().display().to_string(),
            "--branch",
            "main",
        ],
    );
    assert_eq!(linked["id"].as_str().unwrap(), repo_id);
    assert!(!linked["worktrees"].as_array().unwrap().is_empty());
}

#[test]
fn worktree_unlink_resolves_by_prefix() {
    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let repo_id = attach_no_link(&dir, &workspace, "git@github.com:o/r.git", "github.com/o/r");
    let prefix = run_json(&mut bin("repo-link", &dir), &["repo", "show", &repo_id])["prefix"]
        .as_str()
        .unwrap()
        .to_string();

    let repo_dir = TempDir::new().unwrap();
    init_git_repo_with_origin(repo_dir.path(), "git@github.com:o/r.git");
    let path_str = repo_dir.path().display().to_string();

    run_json(
        &mut bin("repo-link", &dir),
        &[
            "worktree", "link", "--repo", &repo_id, "--path", &path_str, "--branch", "main",
        ],
    );

    let unlinked = run_json(
        &mut bin("repo-link", &dir),
        &["worktree", "unlink", "--repo", &prefix, "--path", &path_str],
    );
    assert!(unlinked["worktrees"].as_array().unwrap().is_empty());
}

#[test]
fn worktree_prune_missing_resolves_by_prefix() {
    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let repo_id = attach_no_link(&dir, &workspace, "git@github.com:o/r.git", "github.com/o/r");
    let prefix = run_json(&mut bin("repo-link", &dir), &["repo", "show", &repo_id])["prefix"]
        .as_str()
        .unwrap()
        .to_string();

    // `prune-missing` operates on the stored worktree list — it doesn't
    // probe the filesystem itself; that's `worktree reconcile`. So we don't
    // need a real (or vanished) path here. Any link + prune cycle proves
    // the prefix was resolved to the UUID before the service was called.
    let scratch = TempDir::new().unwrap();
    init_git_repo_with_origin(scratch.path(), "git@github.com:o/r.git");
    let path_str = scratch.path().display().to_string();
    run_json(
        &mut bin("repo-link", &dir),
        &[
            "worktree", "link", "--repo", &repo_id, "--path", &path_str, "--branch", "main",
        ],
    );

    let pruned = run_json(
        &mut bin("repo-link", &dir),
        &["worktree", "prune-missing", "--repo", &prefix],
    );
    assert_eq!(pruned["id"].as_str().unwrap(), repo_id);
}

#[test]
fn worktree_prune_missing_ambiguous_alias_exits_with_candidates() {
    // Two bindings sharing an alias. `worktree prune-missing` must exit 2
    // with the same candidate-JSON shape as `rl repo show`. Prune-missing
    // is the cleanest target because its dispatch only takes `--repo` —
    // no path arg, no canonical discrepancy check fires first. One test
    // stands in for every site because they all route through the single
    // `resolve_repo_handle_required` helper in app-cli.
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
    run_json(
        &mut bin("repo-link", &dir),
        &["repo", "alias", "add", "--repo", &id1, "--alias", "shared"],
    );
    run_json(
        &mut bin("repo-link", &dir),
        &["repo", "alias", "add", "--repo", &id2, "--alias", "shared"],
    );

    let output = bin("repo-link", &dir)
        .args(["worktree", "prune-missing", "--repo", "shared"])
        .assert()
        .failure()
        .get_output()
        .clone();
    // Lock the resolver contract to exit code 2 specifically — a regression
    // to any other non-zero status (e.g. an anyhow bubble at 1) should fail
    // the test rather than silently passing under `assert().failure()`.
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    let body: serde_json::Value =
        serde_json::from_str(&stderr).unwrap_or_else(|e| panic!("not JSON ({e}): {stderr}"));
    assert_eq!(body["error"], "ambiguous");
    assert_eq!(body["candidates"].as_array().unwrap().len(), 2);
}

// ---------- rl task claim ------------------------------------------------

/// Write a two-line token file (`<token>\n<login>\n`) with mode 0o600 so
/// `cfg.resolve_github_login()` returns `Some(login)`. The claim tests need
/// this seed because the cache normally gets populated by `rl gh auth`, but
/// that command makes a real /user round-trip we don't want to mock here.
#[cfg(unix)]
fn seed_token_file(path: &std::path::Path, token: &str, login: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("{token}\n{login}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
#[cfg(unix)]
fn task_claim_local_only_assigns_and_starts_skipping_push() {
    let dir = TempDir::new().unwrap();
    let token_file = dir.path().join("github_token");
    seed_token_file(&token_file, "tok", "benediktms");

    let ws = run_json(
        &mut bin("rl", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();
    let task = run_json(
        &mut bin("rl", &dir),
        &[
            "task",
            "create",
            "--workspace",
            &workspace,
            "--title",
            "claim me",
        ],
    );
    let task_id = task["id"].as_str().unwrap().to_string();

    let mut cmd = bin("rl", &dir);
    cmd.env("REPO_LINK_GITHUB_TOKEN_FILE", &token_file);
    cmd.env_remove("REPO_LINK_GITHUB_TOKEN");
    cmd.env_remove("GITHUB_TOKEN");
    let rows = run_json(&mut cmd, &["task", "claim", "--no-sync", &task_id]);

    let arr = rows.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let row = &arr[0];
    assert_eq!(row["ok"], true);
    assert_eq!(row["push"], "skipped: --no-sync");
    assert_eq!(row["task"]["is_open"], true);
    let assignees: Vec<&str> = row["task"]["assignees"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(assignees, vec!["benediktms"]);
}

#[test]
#[cfg(unix)]
fn task_claim_errors_without_cached_login() {
    let dir = TempDir::new().unwrap();
    // No token file written — `resolve_github_login` returns None.
    let ws = run_json(
        &mut bin("rl", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();
    let task = run_json(
        &mut bin("rl", &dir),
        &[
            "task",
            "create",
            "--workspace",
            &workspace,
            "--title",
            "claim me",
        ],
    );
    let task_id = task["id"].as_str().unwrap().to_string();

    let token_file = dir.path().join("does-not-exist");
    let output = bin("rl", &dir)
        .env("REPO_LINK_GITHUB_TOKEN_FILE", &token_file)
        .env_remove("REPO_LINK_GITHUB_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("REPO_LINK_GITHUB_LOGIN")
        .args(["task", "claim", "--no-sync", &task_id])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("cached GitHub login") && stderr.contains("rl gh auth"),
        "expected login-missing hint; got: {stderr}"
    );
}

#[test]
#[cfg(unix)]
fn task_claim_refuses_done_task() {
    let dir = TempDir::new().unwrap();
    let token_file = dir.path().join("github_token");
    seed_token_file(&token_file, "tok", "benediktms");

    let ws = run_json(
        &mut bin("rl", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();
    let task = run_json(
        &mut bin("rl", &dir),
        &[
            "task",
            "create",
            "--workspace",
            &workspace,
            "--title",
            "done already",
        ],
    );
    let task_id = task["id"].as_str().unwrap().to_string();
    // Drive the task to a closed (completed) state.
    run_json(&mut bin("rl", &dir), &["task", "complete", &task_id]);

    let mut cmd = bin("rl", &dir);
    cmd.env("REPO_LINK_GITHUB_TOKEN_FILE", &token_file);
    cmd.env_remove("REPO_LINK_GITHUB_TOKEN");
    cmd.env_remove("GITHUB_TOKEN");
    let output = cmd
        .args(["task", "claim", "--no-sync", &task_id])
        .assert()
        .failure()
        .get_output()
        .clone();
    // `claim_dispatch` records the per-task error in the JSON array AND
    // bubbles a non-zero exit. The row carries the hint.
    let stdout = String::from_utf8(output.stdout).unwrap();
    let rows: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let row = &rows.as_array().unwrap()[0];
    assert_eq!(row["ok"], false);
    let err_msg = row["error"].as_str().unwrap();
    assert!(
        err_msg.contains("closed") && err_msg.contains("reopen"),
        "expected 'closed; reopen' hint; got: {err_msg}"
    );
}

#[test]
#[cfg(unix)]
fn task_claim_is_idempotent_when_already_claimed() {
    let dir = TempDir::new().unwrap();
    let token_file = dir.path().join("github_token");
    seed_token_file(&token_file, "tok", "benediktms");

    let ws = run_json(
        &mut bin("rl", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();
    let task = run_json(
        &mut bin("rl", &dir),
        &[
            "task",
            "create",
            "--workspace",
            &workspace,
            "--title",
            "claim me twice",
        ],
    );
    let task_id = task["id"].as_str().unwrap().to_string();

    let mut cmd = bin("rl", &dir);
    cmd.env("REPO_LINK_GITHUB_TOKEN_FILE", &token_file);
    cmd.env_remove("REPO_LINK_GITHUB_TOKEN");
    cmd.env_remove("GITHUB_TOKEN");
    let first = run_json(&mut cmd, &["task", "claim", "--no-sync", &task_id]);
    let first_updated_at = first[0]["task"]["updated_at"].clone();
    let first_assignees = first[0]["task"]["assignees"].clone();

    // Second invocation must not mutate the task — already open and
    // the cached login is already in `assignees`, so both branches in
    // `claim_one` are skipped. `updated_at` is the cheap idempotency probe.
    let mut cmd2 = bin("rl", &dir);
    cmd2.env("REPO_LINK_GITHUB_TOKEN_FILE", &token_file);
    cmd2.env_remove("REPO_LINK_GITHUB_TOKEN");
    cmd2.env_remove("GITHUB_TOKEN");
    let second = run_json(&mut cmd2, &["task", "claim", "--no-sync", &task_id]);
    assert_eq!(second[0]["ok"], true);
    assert_eq!(second[0]["task"]["is_open"], true);
    assert_eq!(second[0]["task"]["assignees"], first_assignees);
    assert_eq!(
        second[0]["task"]["updated_at"], first_updated_at,
        "second claim must not touch updated_at",
    );
}

// ---------- RFC 0001 Stage 4 — `rl project` + workspace ↔ project ----------

/// Helper: link a stock project addressed as `acme/7` (node id
/// `PVT_demo_abc`) with two Status options — Backlog and Done.
///
/// `rl project link` fetches the schema over GraphQL (Stage 5), so we stand
/// up a wiremock server returning that schema and point the CLI at it via
/// `REPO_LINK_GITHUB_API_BASE_URL`. Auto-derivation seeds Backlog→open and
/// Done→done. Returns the project node id.
fn link_demo_project(dir: &TempDir) -> String {
    // A multi-thread runtime keeps the mock server serving on background
    // worker threads while the blocking `assert_cmd` subprocess hits it.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            // Only respond once the CLI propagated the parsed `acme/7` target
            // into the request, so the parse->fetch contract is observable here
            // and not just in the adapter tests.
            .and(body_partial_json(
                json!({ "variables": { "owner": "acme", "number": 7 } }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "repositoryOwner": { "projectV2": {
                    "id": "PVT_demo_abc",
                    "number": 7,
                    "title": "Repo Link",
                    "owner": { "login": "acme" },
                    "fields": { "nodes": [
                        { "__typename": "ProjectV2SingleSelectField",
                          "id": "PVTSSF_x", "name": "Status", "options": [
                            { "id": "o1", "name": "Backlog" },
                            { "id": "o2", "name": "Done" }
                        ] }
                    ] }
                } } }
            })))
            .mount(&server)
            .await;
        server
    });

    let mut cmd = bin("repo-link", dir);
    cmd.env("REPO_LINK_GITHUB_API_BASE_URL", server.uri());
    cmd.env("REPO_LINK_GITHUB_TOKEN", "t0k");
    let dto = run_json(&mut cmd, &["project", "link", "acme/7"]);
    assert_eq!(dto["id"], "PVT_demo_abc");
    // `rt`/`server` stay alive until the function returns — i.e. past the
    // blocking link call above.
    dto["id"].as_str().unwrap().to_string()
}

#[test]
fn project_link_rejects_zero_project_number() {
    // `acme/0` is rejected at parse time, before any token/network is needed.
    let dir = TempDir::new().unwrap();
    let output = bin("repo-link", &dir)
        .args(["project", "link", "acme/0"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("positive integer"),
        "expected a positive-integer rejection, got: {stderr}"
    );
}

#[test]
fn project_link_show_and_list_roundtrip() {
    let dir = TempDir::new().unwrap();
    let _id = link_demo_project(&dir);

    let shown = run_json(&mut bin("repo-link", &dir), &["project", "show", "acme/7"]);
    assert_eq!(shown["id"], "PVT_demo_abc");
    assert_eq!(shown["status_options"].as_array().unwrap().len(), 2);
    // Auto-derivation seeds two mappings by name: Backlog→open, Done→done.
    assert_eq!(shown["status_mappings"].as_array().unwrap().len(), 2);

    // `default_for` should also surface inline on the matching option row
    // so a single render covers both the catalog and the mapping.
    let backlog = shown["status_options"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["option_id"] == "o1")
        .unwrap();
    assert_eq!(backlog["default_for"], "open");

    // Show by node id resolves to the same project.
    let by_node = run_json(
        &mut bin("repo-link", &dir),
        &["project", "show", "PVT_demo_abc"],
    );
    assert_eq!(by_node["id"], shown["id"]);

    let listed = run_json(&mut bin("repo-link", &dir), &["project", "list"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);
}

#[test]
fn project_map_appends_then_overwrites() {
    let dir = TempDir::new().unwrap();
    link_demo_project(&dir);

    // RFC 0004: the mapping axis is now the open/closed bit, so a board has at
    // most two mapping rows. Auto-derivation seeds open→o1 and closed→o2.
    // Re-mapping an existing bit overwrites in place — it never appends.
    let after_first = run_json(
        &mut bin("repo-link", &dir),
        &[
            "project",
            "map",
            "acme/7",
            "--local",
            "closed",
            "--option-id",
            "o1",
        ],
    );
    assert_eq!(after_first["status_mappings"].as_array().unwrap().len(), 2);

    // open→o2 overwrites the existing open→o1 mapping — no new row.
    let after_overwrite = run_json(
        &mut bin("repo-link", &dir),
        &[
            "project",
            "map",
            "acme/7",
            "--local",
            "open",
            "--option-id",
            "o2",
        ],
    );
    let mappings = after_overwrite["status_mappings"].as_array().unwrap();
    let open_mapping = mappings.iter().find(|m| m["status"] == "open").unwrap();
    assert_eq!(open_mapping["option_id"], "o2");
    assert_eq!(
        mappings.len(),
        2,
        "overwrite must not append a duplicate row"
    );
}

#[test]
fn project_map_rejects_option_outside_catalog() {
    let dir = TempDir::new().unwrap();
    link_demo_project(&dir);

    let output = bin("repo-link", &dir)
        .args([
            "project",
            "map",
            "acme/7",
            "--local",
            "open",
            "--option-id",
            "ghost",
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("ghost"),
        "error should name the offending option_id: {stderr}"
    );
}

#[test]
fn project_unlink_removes_and_clears_workspace_membership() {
    let dir = TempDir::new().unwrap();
    link_demo_project(&dir);

    // Create a workspace attached to the project.
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &[
            "workspace",
            "create",
            "bound",
            "--local-only",
            "--project",
            "acme/7",
        ],
    );
    assert_eq!(ws["project_id"], "PVT_demo_abc");

    // Unlink the project locally.
    let result = run_json(
        &mut bin("repo-link", &dir),
        &["project", "unlink", "acme/7"],
    );
    assert_eq!(result["unlinked"], "acme/7");

    // The workspace survives but its project_id is cleared
    // (`workspaces.project_id` is `ON DELETE SET NULL`).
    let reloaded = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "show", ws["id"].as_str().unwrap()],
    );
    assert!(
        reloaded.get("project_id").is_none()
            || reloaded["project_id"].is_null()
            || reloaded["project_id"].as_str().is_none(),
        "workspace must lose its project_id after the project is unlinked: {reloaded}"
    );
}

#[test]
fn workspace_set_project_attaches_and_detaches() {
    let dir = TempDir::new().unwrap();
    link_demo_project(&dir);

    // Start projectless, attach, then detach.
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let ws_id = ws["id"].as_str().unwrap().to_string();
    assert!(ws.get("project_id").is_none() || ws["project_id"].is_null());

    let attached = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "set-project", &ws_id, "--project", "acme/7"],
    );
    assert_eq!(attached["project_id"], "PVT_demo_abc");

    let detached = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "set-project", &ws_id, "--none"],
    );
    assert!(
        detached.get("project_id").is_none() || detached["project_id"].is_null(),
        "detached workspace must have no project_id: {detached}"
    );
}

#[test]
fn workspace_set_project_requires_one_of_project_or_none() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let ws_id = ws["id"].as_str().unwrap().to_string();

    let output = bin("repo-link", &dir)
        .args(["workspace", "set-project", &ws_id])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("--project") || stderr.contains("--none"),
        "error should mention the required flags: {stderr}"
    );
}

// ---------- workspace set-filing-repo (RFC 0002 §4 / GitHub #121) ----------

#[test]
fn workspace_set_filing_repo_attaches_by_name() {
    let dir = TempDir::new().unwrap();

    let ws_id = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // canonical "github.com/o/r" → derived name is "r"
    let binding_id = attach_no_link(&dir, &ws_id, "git@github.com:o/r.git", "github.com/o/r");

    let dto = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "set-filing-repo", &ws_id, "--repo", "r"],
    );
    // RFC 0005 §D4: the filing axis is origin-level — the recorded value is the
    // shared ORIGIN id, not the per-workspace instance id `attach_no_link`
    // returns (the promote path reads it back as an origin id).
    assert_eq!(
        dto["filing_repo_id"].as_str().unwrap(),
        origin_id_of(&dir, "r"),
        "filing_repo_id must equal the repo's origin id"
    );
    assert_ne!(
        dto["filing_repo_id"].as_str().unwrap(),
        binding_id,
        "filing_repo_id must NOT be the instance id"
    );
}

#[test]
fn workspace_set_filing_repo_none_clears_it() {
    let dir = TempDir::new().unwrap();

    let ws_id = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    let _binding_id = attach_no_link(&dir, &ws_id, "git@github.com:o/r.git", "github.com/o/r");

    // Set, then clear.
    run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "set-filing-repo", &ws_id, "--repo", "r"],
    );
    let cleared = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "set-filing-repo", &ws_id, "--none"],
    );
    assert!(
        cleared.get("filing_repo_id").is_none() || cleared["filing_repo_id"].is_null(),
        "after --none, filing_repo_id must be absent/null: {cleared}"
    );
}

#[test]
fn workspace_set_filing_repo_reassignment_succeeds() {
    // Unlike set-project, reassigning from repo A to repo B is ALLOWED.
    let dir = TempDir::new().unwrap();

    let ws_id = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    attach_no_link(&dir, &ws_id, "git@github.com:o/a.git", "github.com/o/a");
    attach_no_link(&dir, &ws_id, "git@github.com:o/b.git", "github.com/o/b");

    run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "set-filing-repo", &ws_id, "--repo", "a"],
    );
    let dto = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "set-filing-repo", &ws_id, "--repo", "b"],
    );
    // RFC 0005 §D4: filing is recorded as the origin id (see above).
    assert_eq!(
        dto["filing_repo_id"].as_str().unwrap(),
        origin_id_of(&dir, "b"),
        "after reassignment, dto must reflect repo B's origin id"
    );
    assert_ne!(
        dto["filing_repo_id"].as_str().unwrap(),
        origin_id_of(&dir, "a"),
        "dto must no longer show repo A after reassignment"
    );
}

#[test]
fn workspace_set_filing_repo_requires_repo_or_none() {
    let dir = TempDir::new().unwrap();

    let ws_id = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    let output = bin("repo-link", &dir)
        .args(["workspace", "set-filing-repo", &ws_id])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("--repo") || stderr.contains("--none"),
        "error should mention the required flags: {stderr}"
    );
}

/// `rl repo doctor` is the user-initiated repair verb for rpl-sv2 (the
/// silent-divergence bug where a binding is deleted out from under a
/// task's recorded `filing_repo_id`). On a healthy workspace (no
/// affected tasks), the command emits the doctor envelope with
/// `affected: 0` — this verifies the CLI wiring (the deeper logic
/// lives in the `application-workspace` unit tests, which can plant a
/// task with a `filing_repo_id` via `force_set_filing_repo_id`
/// without needing a real GitHub promote roundtrip).
#[test]
fn repo_doctor_emits_zero_envelope_on_healthy_workspace() {
    let dir = TempDir::new().unwrap();
    let ws = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    );
    let workspace = ws["id"].as_str().unwrap().to_string();

    // Seed a binding so the workspace isn't empty, but with no tasks
    // attached (the doctor's only interest).
    let repo_dir = TempDir::new().unwrap();
    init_git_repo_with_origin(repo_dir.path(), "git@github.com:o/r.git");
    let repo_path = repo_dir.path().display().to_string();
    run_json(
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

    // List-only mode (no `--repair`).
    let list_only = run_json(
        &mut bin("repo-link", &dir),
        &["repo", "doctor", "--workspace", &workspace],
    );
    assert_eq!(list_only["affected"], 0);
    assert_eq!(list_only["repaired"], 0);
    assert_eq!(list_only["unresolved"], 0);
    assert!(list_only["rows"].as_array().unwrap().is_empty());

    // With `--repair` (and no affected tasks): same envelope, no error.
    let repair_noop = run_json(
        &mut bin("repo-link", &dir),
        &["repo", "doctor", "--workspace", &workspace, "--repair"],
    );
    assert_eq!(repair_noop["affected"], 0);
    assert_eq!(repair_noop["repaired"], 0);
}

/// RFC 0005 §D4: `repo doctor --target <handle>` re-points the filing axis
/// (origin id space). The handle must resolve to the repo's ORIGIN id — the
/// doctor pre-validates the override via `get_origin` before the task loop, so
/// passing the per-workspace INSTANCE id (which differs from the origin id for
/// a freshly-attached repo) would reject a perfectly valid target. No dangling
/// task is needed to trip the bug: override validation runs even on a clean ws.
#[test]
fn repo_doctor_target_resolves_origin_not_instance() {
    let dir = TempDir::new().unwrap();
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "w", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let repo = attach_no_link(&dir, &workspace, "git@github.com:o/r.git", "github.com/o/r");
    // Precondition for the bug: fresh attach => instance id != origin id.
    assert_ne!(origin_id_of(&dir, &repo), repo);

    // Before the fix this exits non-zero (get_origin(instance_id) => NotFound);
    // run_json asserts success, so a green run proves the origin-id resolution.
    let summary = run_json(
        &mut bin("repo-link", &dir),
        &[
            "repo",
            "doctor",
            "--workspace",
            &workspace,
            "--target",
            &repo,
        ],
    );
    assert_eq!(summary["affected"], 0);
}

/// `--workspace` is optional: when omitted, it's derived from the current
/// directory's repo (cwd git origin → the workspace that has it attached).
/// A repo in exactly one workspace resolves cleanly.
#[test]
fn workspace_arg_derives_from_cwd_when_single_workspace() {
    let dir = TempDir::new().unwrap();
    // A decoy workspace created FIRST and unrelated to the checkout. If
    // resolution wrongly fell back to "the only / first workspace in the DB" it
    // would pick this one — the assertion below pins that it resolves from the
    // checkout's repo binding instead.
    run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "decoy", "--local-only"],
    );
    let workspace = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "target", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    let checkout = TempDir::new().unwrap();
    init_git_repo_with_origin(checkout.path(), "git@github.com:o/r.git");
    let checkout_path = checkout.path().display().to_string();
    // Attach the checkout's repo to the TARGET workspace only.
    run_json(
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
            "--path",
            &checkout_path,
        ],
    );

    // `query overview` from inside the checkout with NO --workspace must resolve
    // to the TARGET workspace (bound to this checkout), not the decoy.
    let mut cmd = bin("repo-link", &dir);
    cmd.current_dir(checkout.path());
    let out = run_json(&mut cmd, &["query", "overview"]);
    assert_eq!(
        out["workspace_id"].as_str(),
        Some(workspace.as_str()),
        "cwd derivation must select the checkout's workspace, not a fallback: {out}"
    );
    assert_eq!(out["workspace_name"], "target");
}

/// When the cwd repo is attached to more than one workspace, derivation is
/// ambiguous and the command errors asking for `--workspace` rather than
/// picking one.
#[test]
fn workspace_arg_errors_when_cwd_repo_spans_multiple_workspaces() {
    let dir = TempDir::new().unwrap();
    let ws_a = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "a", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let ws_b = run_json(
        &mut bin("repo-link", &dir),
        &["workspace", "create", "b", "--local-only"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    let checkout = TempDir::new().unwrap();
    init_git_repo_with_origin(checkout.path(), "git@github.com:o/r.git");
    let checkout_path = checkout.path().display().to_string();
    for ws in [&ws_a, &ws_b] {
        run_json(
            &mut bin("repo-link", &dir),
            &[
                "repo",
                "attach",
                "--workspace",
                ws,
                "--url",
                "git@github.com:o/r.git",
                "--canonical",
                "github.com/o/r",
                "--path",
                &checkout_path,
            ],
        );
    }

    let mut cmd = bin("repo-link", &dir);
    cmd.current_dir(checkout.path());
    let output = cmd
        .args(["query", "overview"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("--workspace"),
        "ambiguous cwd must ask for --workspace, got: {stderr}"
    );
}
