//! `rl task` dispatch, plus the batch / claim machinery and the
//! `git_user_name` helper (also used by `query mine`).

use anyhow::{Result, anyhow};
use application_sync::SyncService;
use dto_shared::{
    AddTaskRelationCmd, CreateTaskCmd, ListTasksQuery, RemoveTaskRelationCmd, TaskDto,
    UpdateTaskCmd,
};
use infra_config::RepoLinkConfig;

use crate::cli::{TaskCmd, WorkspaceArg};
use crate::commands::repo::{resolve_repo_handle, resolve_repo_handle_required};
use crate::commands::sync::parse_issue_url;
use crate::render;
use crate::services::{Services, build_sync_service};

/// Read `git config user.name` from the surrounding git repo. Returns
/// `None` if git isn't on PATH, the cwd isn't inside a repo, or the value
/// is empty. Used as a sensible default for `query mine --assignee`.
pub(crate) fn git_user_name() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["config", "user.name"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Apply a per-task service op to a batch of IDs and emit a per-task
/// success/error JSON array. We don't bail on the first failure so the
/// caller can see partial progress — a missing or stale ID in the middle
/// shouldn't hide what worked.
async fn batch_task_op<F, Fut>(tasks: Vec<String>, mut op: F) -> Result<()>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<
            Output = std::result::Result<dto_shared::TaskDto, application_task::ServiceError>,
        >,
{
    let mut results: Vec<serde_json::Value> = Vec::with_capacity(tasks.len());
    let mut had_errors = false;
    let mut failed_ids: Vec<String> = Vec::new();
    for id in tasks {
        let recorded = id.clone();
        match op(id).await {
            Ok(dto) => results.push(serde_json::json!({
                "task_id": recorded,
                "ok": true,
                "task": dto,
            })),
            Err(e) => {
                had_errors = true;
                failed_ids.push(recorded.clone());
                results.push(serde_json::json!({
                    "task_id": recorded,
                    "ok": false,
                    "error": e.to_string(),
                }));
            }
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&results).unwrap_or_else(|_| "[]".into())
    );
    if had_errors {
        return Err(anyhow!(
            "batch had {} failed task(s): {}",
            failed_ids.len(),
            failed_ids.join(", ")
        ));
    }
    Ok(())
}

/// Drive `rl task claim` across a batch. Mirrors [`batch_task_op`]'s output
/// shape (`task_id` / `ok` / `task` | `error`) and adds a `push` field so the
/// caller can see whether the GitHub round-trip happened.
async fn claim_dispatch(
    svc: &Services,
    cfg: &RepoLinkConfig,
    tasks: Vec<String>,
    no_sync: bool,
) -> Result<()> {
    // Front-load both the login and the sync service so a misconfiguration
    // errors before mutating any task state. The whole batch shares one
    // SyncService instance.
    let login = cfg
        .resolve_github_login()
        .map_err(|e| anyhow!("{e}"))?
        .ok_or_else(|| {
            anyhow!(
                "rl task claim needs the cached GitHub login. \
                 Run `rl gh auth` (with network access + a valid token) \
                 so the login can be cached."
            )
        })?;
    let sync = if no_sync {
        None
    } else {
        Some(build_sync_service(cfg, svc, "task claim")?)
    };

    let mut rows: Vec<serde_json::Value> = Vec::with_capacity(tasks.len());
    let mut had_errors = false;
    let mut failed_ids: Vec<String> = Vec::new();
    for task_ref in tasks {
        let recorded = task_ref.clone();
        match claim_one(svc, sync.as_ref(), &task_ref, &login).await {
            Ok((dto, push)) => rows.push(serde_json::json!({
                "task_id": recorded,
                "ok": true,
                "task": dto,
                "push": push,
            })),
            Err(e) => {
                had_errors = true;
                failed_ids.push(recorded.clone());
                rows.push(serde_json::json!({
                    "task_id": recorded,
                    "ok": false,
                    "error": e.to_string(),
                }));
            }
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".into())
    );
    if had_errors {
        return Err(anyhow!(
            "batch had {} failed task(s): {}",
            failed_ids.len(),
            failed_ids.join(", ")
        ));
    }
    Ok(())
}

/// One iteration of `rl task claim`. Refuses on `Done` / `Archived`; for
/// everything else the pipeline is merge-then-start-then-push. Idempotent:
/// if the login is already in `assignees` AND the task is already
/// in-progress, the push step is reported as `noop`.
async fn claim_one(
    svc: &Services,
    sync: Option<&SyncService>,
    task_ref: &str,
    login: &str,
) -> Result<(dto_shared::TaskDto, String)> {
    let task_id = svc.tasks.resolve_id(task_ref).await?;
    let mut dto = svc.tasks.show(&task_id).await?;

    match dto.status.as_str() {
        "done" => {
            return Err(anyhow!(
                "task {task_ref} is done; reopen it before claiming"
            ));
        }
        "archived" => {
            return Err(anyhow!(
                "task {task_ref} is archived; unarchive it before claiming"
            ));
        }
        _ => {}
    }

    let need_assign = !dto.assignees.iter().any(|a| a == login);
    let need_start = matches!(dto.status.as_str(), "open" | "blocked");

    if need_assign {
        let mut next = dto.assignees.clone();
        next.push(login.to_string());
        dto = svc
            .tasks
            .update(UpdateTaskCmd {
                task_id: task_id.clone(),
                title: None,
                body: None,
                priority: None,
                assignees: Some(next),
                repo_id: None,
            })
            .await
            .map_err(|e| anyhow!("{e}"))?;
    }
    if need_start {
        dto = svc
            .tasks
            .start(&task_id)
            .await
            .map_err(|e| anyhow!("{e}"))?;
    }

    let push = match sync {
        None => "skipped: --no-sync".to_string(),
        Some(_) if dto.remote.is_none() => "skipped: not promoted".to_string(),
        Some(_) if !need_assign && !need_start => "noop".to_string(),
        Some(s) => match s.push(&task_id).await {
            Ok(_) => "synced".to_string(),
            Err(e) => format!("failed: {e}"),
        },
    };
    Ok((dto, push))
}

pub(crate) async fn task_dispatch(
    cmd: TaskCmd,
    svc: &Services,
    cfg: &RepoLinkConfig,
) -> Result<()> {
    match cmd {
        TaskCmd::Create {
            ws: WorkspaceArg { workspace },
            repo,
            filing_repo,
            title,
            body,
            priority,
        } => {
            // RFC 0002 D2 step 1 / #122 brief preference (a): `task create`
            // only mints a LocalOnly draft — it does not promote and has no
            // filing transition to consume the override. Silently accepting
            // `--filing-repo` would create a flag that does nothing (a
            // footgun). Instead, resolve the handle first (to validate it and
            // surface ambiguity identically to `--repo`), then reject with an
            // explicit deferral error directing the user to `rl sync promote`
            // / a future workspace filing default.
            if let Some(handle) = filing_repo {
                let resolved = resolve_repo_handle_required(svc, &handle).await?;
                return Err(anyhow!(
                    "`--filing-repo` is not yet consumed by `task create` (RFC 0002 §4, #122): \
                     `rl task create` only mints a local draft and does not promote the task to \
                     a remote issue. The per-task filing-repo override will be honoured at the \
                     first-filing transition; until that path is wired, control the filing target \
                     via the workspace filing default. To file the task in a specific repo, \
                     create it without `--filing-repo` and then run `rl sync promote`. \
                     (Resolved binding: {})",
                    resolved
                ));
            }
            let dto = svc
                .tasks
                .create(CreateTaskCmd {
                    workspace_id: workspace,
                    repo_id: resolve_repo_handle(svc, repo).await?,
                    title,
                    body,
                    priority,
                    filing_repo_override: None,
                })
                .await?;
            render::task(&dto);
        }
        TaskCmd::Show { id } => {
            // Show-specific display path (RFC 0002 D5 / #122): read the
            // domain Task directly for the internal `filing_repo_id` axis,
            // then overlay an additive `filing_repo` block on top of the
            // base TaskDto — without extending TaskDto itself. The base
            // shape (and all list/query shapes) remain byte-identical.
            let base = svc.tasks.show(&id).await?;
            let domain_task = svc.tasks.resolve_task(&id).await?;
            let filing_repo_block = if let Some(filing_id) = domain_task.filing_repo_id {
                match svc.bindings.show(&filing_id.to_string()).await {
                    Ok(binding) => serde_json::json!({
                        "id": binding.id,
                        "name": binding.name,
                        "canonical_url": binding.canonical_url,
                    }),
                    // A recorded filing repo whose binding was since deleted is
                    // a legitimate dangling pointer (no FK on filing_repo_id) —
                    // surface it as null. Any OTHER error (I/O, etc.) must
                    // propagate, not be silently masked as "no filing repo".
                    Err(application_workspace::ServiceError::BindingNotFound(_)) => {
                        serde_json::Value::Null
                    }
                    Err(e) => return Err(anyhow!("resolve filing repo binding: {e}")),
                }
            } else {
                serde_json::Value::Null
            };
            render::task_show(&base, filing_repo_block);
        }
        TaskCmd::Edit {
            id,
            title,
            body,
            priority,
            assignees,
            repo,
        } => {
            // Reject the empty case at the CLI boundary. The service layer
            // intentionally accepts a no-op UpdateTaskCmd (a future API
            // binding may want a touch-only refresh) — the `rl task edit`
            // command's contract is stricter.
            if title.is_none()
                && body.is_none()
                && priority.is_none()
                && assignees.is_empty()
                && repo.is_none()
            {
                return Err(anyhow!(
                    "rl task edit requires at least one of --title, --body, --priority, --assignee, --repo"
                ));
            }
            // Collapse clap's accumulated Vec into the DTO's "None = no
            // change" shape. The trade-off is that "clear all assignees"
            // is unreachable via `edit`; the spec explicitly accepts this.
            let dto = svc
                .tasks
                .update(UpdateTaskCmd {
                    task_id: id,
                    title,
                    body,
                    priority,
                    assignees: (!assignees.is_empty()).then_some(assignees),
                    repo_id: resolve_repo_handle(svc, repo).await?,
                })
                .await?;
            render::task(&dto);
        }
        TaskCmd::List {
            workspace,
            status,
            sync_state,
            include_archived,
        } => {
            let rows = svc
                .tasks
                .list(ListTasksQuery {
                    workspace_id: workspace,
                    repo_id: None,
                    status,
                    sync_state,
                    include_archived,
                })
                .await?;
            render::tasks(&rows);
        }
        TaskCmd::Stage { tasks } => {
            batch_task_op(
                tasks,
                |id| async move { svc.tasks.stage_for_sync(&id).await },
            )
            .await?;
        }
        TaskCmd::Start { tasks } => {
            batch_task_op(tasks, |id| async move { svc.tasks.start(&id).await }).await?;
        }
        TaskCmd::Complete { tasks } => {
            batch_task_op(tasks, |id| async move { svc.tasks.complete(&id).await }).await?;
        }
        TaskCmd::Reopen { tasks } => {
            batch_task_op(tasks, |id| async move { svc.tasks.reopen(&id).await }).await?;
        }
        TaskCmd::Block { tasks } => {
            batch_task_op(tasks, |id| async move { svc.tasks.mark_blocked(&id).await }).await?;
        }
        TaskCmd::Unblock { tasks } => {
            batch_task_op(tasks, |id| async move { svc.tasks.unblock(&id).await }).await?;
        }
        TaskCmd::Archive { tasks } => {
            batch_task_op(tasks, |id| async move { svc.tasks.archive(&id).await }).await?;
        }
        TaskCmd::Claim { tasks, no_sync } => {
            claim_dispatch(svc, cfg, tasks, no_sync).await?;
        }
        TaskCmd::Comment { id, body } => {
            // Provisional local author (same precedence as `query mine`); the
            // real author is filled in from GitHub when the comment is pushed.
            let author = git_user_name()
                .or_else(|| std::env::var("REPO_LINK_USER").ok())
                .or_else(|| std::env::var("USER").ok())
                .unwrap_or_else(|| "local".into());
            let dto = svc.tasks.add_comment(&id, &body, &author).await?;
            render::task(&dto);
        }
        TaskCmd::Link { id, url, relink } => {
            let (canonical, remote_id) =
                parse_issue_url(&url).ok_or_else(|| anyhow!("not a github issue url: {url}"))?;
            let task_id = svc.tasks.resolve_id(&id).await?;
            let sync = build_sync_service(cfg, svc, "task link")?;
            let summary = sync.link(&task_id, &canonical, &remote_id, relink).await?;
            render::sync(&summary);
        }
        TaskCmd::Relate {
            id,
            kind,
            other,
            remove,
        } => {
            // Run the service call inside a String-typed result so we can
            // surface failures as a structured `{ok, error}` envelope on
            // stdout (mirroring the per-row shape used by the batch
            // lifecycle commands) rather than the bare `TaskDto` we used
            // to emit on success. The clap-validation cases (bad arg
            // combos) stay as `anyhow!` returns — those are caller errors
            // that should not produce a relate-shaped envelope.
            let result: Result<TaskDto, String> = match (remove, kind, other) {
                // Add a single edge (the default).
                (false, Some(k), Some(o)) => svc
                    .tasks
                    .add_relation(AddTaskRelationCmd {
                        task_id: id,
                        kind: k.as_kind_str().to_string(),
                        other: o,
                    })
                    .await
                    .map_err(|e| e.to_string()),
                (false, _, _) => {
                    return Err(anyhow!(
                        "relate requires --kind and --other (or pass --remove to delete)"
                    ));
                }
                // Remove a single edge.
                (true, Some(k), Some(o)) => svc
                    .tasks
                    .remove_relation(RemoveTaskRelationCmd {
                        task_id: id,
                        kind: k.as_kind_str().to_string(),
                        other: o,
                    })
                    .await
                    .map_err(|e| e.to_string()),
                // Remove all relations on the task.
                (true, None, None) => svc
                    .tasks
                    .clear_relations(&id)
                    .await
                    .map_err(|e| e.to_string()),
                (true, _, _) => {
                    return Err(anyhow!(
                        "--remove takes either both --kind and --other (one edge) or neither (all relations)"
                    ));
                }
            };

            match result {
                Ok(dto) => {
                    let body = serde_json::json!({ "ok": true, "task": dto });
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&body)
                            .unwrap_or_else(|_| r#"{"ok":true}"#.to_string())
                    );
                }
                Err(msg) => {
                    let body = serde_json::json!({ "ok": false, "error": msg });
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&body)
                            .unwrap_or_else(|_| format!(r#"{{"ok":false,"error":{msg}}}"#))
                    );
                    // Mirror the error on stderr and exit 1 directly
                    // (matching `handle_ambiguous` in commands/mod.rs) so
                    // shell pipelines see a single, clean stderr line
                    // rather than the duplicate `Error: relate failed: ...`
                    // line that anyhow's Termination impl would add if
                    // we returned `Err(anyhow!(...))` and let the bin
                    // shim's default Result handling take over.
                    eprintln!("error: {msg}");
                    std::process::exit(1);
                }
            }
        }
        TaskCmd::Snapshots { id } => {
            let snaps = svc
                .tasks
                .list_snapshots(&id)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            render::snapshots(&snaps);
        }
        TaskCmd::Rollback { id, to_version } => {
            let dto = svc
                .tasks
                .rollback(&id, to_version)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            render::task(&dto);
        }
    }
    Ok(())
}
