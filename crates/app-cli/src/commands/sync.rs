//! `rl sync` dispatch: promote / push / pull reconciliation plus `import`,
//! and the GitHub issue-URL parser shared with `rl task link`.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use application_sync::SyncService;
use dto_shared::{AddTaskRelationCmd, ImportMirrorCmd};
use infra_config::RepoLinkConfig;

use crate::cli::{SyncCmd, TaskArg, WorkspaceArg};
use crate::render;
use crate::services::{Services, build_github_provider, require_github_token};

/// If a sync verb (push / pull / promote) failed because the issue was
/// transferred on GitHub, suffix the bare port error with the exact
/// `rl task link --relink` command the user should run next.
fn enrich_issue_moved(task_ref: &str, err: application_sync::SyncError) -> anyhow::Error {
    if let application_sync::SyncError::Port(ports::PortError::IssueMoved {
        to_canonical,
        to_remote_id,
        ..
    }) = &err
    {
        let repo = to_canonical.trim_start_matches("github.com/");
        return anyhow!(
            "{err}\n\nThe issue was transferred. Re-link with:\n  \
             rl task link --relink {task_ref} https://github.com/{repo}/issues/{to_remote_id}"
        );
    }
    anyhow!("{err}")
}

pub(crate) async fn sync_dispatch(
    cmd: SyncCmd,
    svc: &Services,
    cfg: &RepoLinkConfig,
) -> Result<()> {
    // `outbox` is a local read of the dead-letter queue — no token, no
    // network. Handle it before the token gate so it works offline.
    if let SyncCmd::Outbox = cmd {
        return sync_outbox(svc).await;
    }

    let token = require_github_token(cfg, "sync")?;
    let provider: Arc<dyn ports::RemoteTaskProvider> =
        Arc::new(build_github_provider(&token, cfg).map_err(|e| anyhow!("{e}"))?);

    // `import` has its own orchestration (fetch + materialise), not the
    // promote/push/pull reconciliation `SyncService` handles.
    if let SyncCmd::Import {
        url,
        cascade,
        ws: WorkspaceArg { workspace },
    } = cmd
    {
        return sync_import(provider.as_ref(), svc, &workspace, &url, cascade).await;
    }

    let sync = SyncService::new(svc.tasks_repo.clone(), svc.bindings_repo.clone(), provider);
    // Resolve the friendly task reference (UUID / bare hash / prefix-hash)
    // to a UUID here, at the CLI boundary, so `sync` accepts the same id
    // forms as every other task command. `SyncService` stays UUID-only.
    let summary = match cmd {
        SyncCmd::Promote {
            t: TaskArg { task },
        } => {
            let id = svc.tasks.resolve_id(&task).await?;
            sync.promote(&id)
                .await
                .map_err(|e| enrich_issue_moved(&task, e))?
        }
        SyncCmd::Push {
            t: TaskArg { task },
        } => {
            let id = svc.tasks.resolve_id(&task).await?;
            sync.push(&id)
                .await
                .map_err(|e| enrich_issue_moved(&task, e))?
        }
        SyncCmd::Pull {
            t: TaskArg { task },
        } => {
            let id = svc.tasks.resolve_id(&task).await?;
            sync.pull(&id)
                .await
                .map_err(|e| enrich_issue_moved(&task, e))?
        }
        SyncCmd::Import { .. } | SyncCmd::Outbox => unreachable!("handled above"),
    };
    render::sync(&summary);
    Ok(())
}

/// Render the dead-letter queue as a JSON array. Minimal by design (#54):
/// one row per failed entry with the fields a human needs to triage — the
/// task, the mutation kind, attempt count, and last error. Polishing this
/// into a richer `rl sync status` surface (live pending counts, per-task
/// breakdown) is deferred.
async fn sync_outbox(svc: &Services) -> Result<()> {
    let dead = svc
        .outbox_repo
        .list_dead_lettered()
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let payload = dead_letter_json(&dead);
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into())
    );
    Ok(())
}

/// Shape the dead-letter queue into the `rl sync outbox` JSON payload:
/// `{dead_lettered, entries:[{id,task_id,kind,attempts,last_error,updated_at}]}`.
/// Pure (no I/O) so the JSON contract is unit-testable without standing up the
/// SQLite-backed `Services`.
fn dead_letter_json(dead: &[domain_sync::OutboxEntry]) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = dead
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.id.to_string(),
                "task_id": e.task_id.to_string(),
                "kind": e.mutation.kind(),
                "attempts": e.attempts,
                "last_error": e.last_error,
                "updated_at": e.updated_at.into_inner(),
            })
        })
        .collect();
    serde_json::json!({
        "dead_lettered": rows.len(),
        "entries": rows,
    })
}

/// Parse a GitHub issue URL into `(canonical "github.com/owner/repo", number)`.
/// Returns `None` for anything that isn't a `github.com/.../issues/<n>` URL.
pub(crate) fn parse_issue_url(url: &str) -> Option<(String, String)> {
    let trimmed = url.trim().trim_end_matches('/');
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    let mut parts = rest.split('/');
    if parts.next()? != "github.com" {
        return None;
    }
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    if parts.next()? != "issues" {
        return None;
    }
    let number = parts.next()?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Reject trailing segments (e.g. .../issues/1/foo) so we don't silently
    // accept a malformed URL.
    if parts.next().is_some() {
        return None;
    }
    Some((format!("github.com/{owner}/{repo}"), number.to_string()))
}

/// Import a GitHub issue (and optionally its sub-issue tree) into local mirror
/// tasks. The whole tree lands under the root issue's repo binding; sub-issues
/// in a different repo are skipped. Idempotent: issues already tracked locally
/// are reported, not re-created. Emits a `batch_task_op`-style JSON array.
async fn sync_import(
    provider: &dyn ports::RemoteTaskProvider,
    svc: &Services,
    workspace: &str,
    url: &str,
    cascade: bool,
) -> Result<()> {
    const PROVIDER: &str = "github";
    const MAX_DEPTH: usize = 25;

    let workspace_id: domain_core::WorkspaceId = workspace
        .parse()
        .map_err(|e| anyhow!("invalid workspace id: {e}"))?;
    let (root_canonical, root_number) =
        parse_issue_url(url).ok_or_else(|| anyhow!("not a github issue url: {url}"))?;

    let root_binding = svc
        .bindings_repo
        .find_by_canonical_url(workspace_id, &root_canonical)
        .await
        .map_err(|e| anyhow!("{e}"))?
        .ok_or_else(|| {
            anyhow!(
                "no repo binding for {root_canonical} in this workspace; \
                 attach it first with `rl repo attach`"
            )
        })?;
    let repo_id = root_binding.id; // RepoId — all imported tasks land under it
    let repo_id_str = repo_id.to_string();

    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    // DFS: (canonical, number, parent_task_id, depth). Parent imported before
    // its children, so the `child_of` target always exists when wired.
    let mut stack: Vec<(String, String, Option<String>, usize)> =
        vec![(root_canonical.clone(), root_number, None, 0)];

    while let Some((canonical, number, parent_id, depth)) = stack.pop() {
        if !visited.insert(number.clone()) {
            continue; // cycle guard
        }

        // Resolve the task id for this node: either the already-tracked one,
        // or a freshly imported mirror. Used to parent its children.
        let node_task_id = if let Some(existing) = svc
            .tasks_repo
            .find_by_remote(repo_id, PROVIDER, &number)
            .await
            .map_err(|e| anyhow!("{e}"))?
        {
            results.push(serde_json::json!({
                "remote_id": number, "ok": false, "reason": "already_tracked",
                "task_id": existing.id.to_string(),
            }));
            existing.id.to_string()
        } else {
            let snap = provider
                .fetch_remote(&canonical, &number)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            let cmd = ImportMirrorCmd {
                workspace_id: workspace.to_string(),
                repo_id: Some(repo_id_str.clone()),
                provider: PROVIDER.to_string(),
                remote_id: number.clone(),
                title: snap.title,
                body: snap.body,
                assignees: snap.assignees,
                closed: snap.closed,
            };
            match svc.tasks.import_mirror(cmd).await {
                Ok(dto) => {
                    results.push(serde_json::json!({
                        "remote_id": number, "ok": true, "task_id": dto.id, "title": dto.title,
                    }));
                    dto.id
                }
                // Race: another writer inserted this remote between our
                // find_by_remote check and the save. The repo-scoped UNIQUE
                // means the conflict is genuinely the same remote object, so
                // treat it as an idempotent already-tracked rather than erroring.
                Err(application_task::ServiceError::Port(ports::PortError::Conflict {
                    ..
                })) => {
                    match svc
                        .tasks_repo
                        .find_by_remote(repo_id, PROVIDER, &number)
                        .await
                        .map_err(|e| anyhow!("{e}"))?
                    {
                        Some(existing) => {
                            results.push(serde_json::json!({
                                "remote_id": number, "ok": false, "reason": "already_tracked",
                                "task_id": existing.id.to_string(),
                            }));
                            existing.id.to_string()
                        }
                        None => {
                            return Err(anyhow!(
                                "remote {number} conflicted on save but no local task found"
                            ));
                        }
                    }
                }
                Err(e) => return Err(anyhow!("{e}")),
            }
        };

        // Wire the parent link regardless of how the node was resolved (fresh
        // import, already-tracked, or conflict recovery), so re-running
        // `--cascade` can't leave a previously-imported child disconnected
        // from its parent. `add_relation` dedups, so this is idempotent.
        if let Some(parent) = &parent_id {
            svc.tasks
                .add_relation(AddTaskRelationCmd {
                    task_id: node_task_id.clone(),
                    kind: "child_of".to_string(),
                    other: parent.clone(),
                })
                .await
                .map_err(|e| anyhow!("{e}"))?;
        }

        if cascade && depth < MAX_DEPTH {
            let children = provider
                .fetch_sub_issues(&canonical, &number)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            for child in children {
                // The whole tree lives under the root binding; a child in a
                // different repo can't be filed here, so skip + report it.
                if child.canonical_repo != root_canonical {
                    results.push(serde_json::json!({
                        "remote_id": child.snapshot.remote_id, "ok": false,
                        "reason": "skipped_cross_repo", "repo": child.canonical_repo,
                    }));
                    continue;
                }
                stack.push((
                    root_canonical.clone(),
                    child.snapshot.remote_id,
                    Some(node_task_id.clone()),
                    depth + 1,
                ));
            }
        }
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&results).unwrap_or_else(|_| "[]".into())
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{dead_letter_json, parse_issue_url};

    #[test]
    fn dead_letter_json_shape_and_count() {
        use domain_core::TaskId;
        use domain_sync::{OutboxEntry, OutboxMutation, OutboxStatus};

        let task_id = TaskId::new();
        let mut e = OutboxEntry::new(
            task_id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "42".into(),
                title: None,
                body: None,
                closed: None,
            },
        );
        // Shape it like a dead-lettered row.
        e.status = OutboxStatus::Failed;
        e.attempts = 5;
        e.last_error = Some("graphql 5xx".into());

        let v = dead_letter_json(std::slice::from_ref(&e));

        assert_eq!(v["dead_lettered"], 1);
        let entries = v["entries"].as_array().expect("entries is an array");
        assert_eq!(entries.len(), 1);
        let row = &entries[0];
        // All documented keys are present with the expected values.
        assert_eq!(row["id"], e.id.to_string());
        assert_eq!(row["task_id"], task_id.to_string());
        assert_eq!(row["kind"], "update_remote");
        assert_eq!(row["attempts"], 5);
        assert_eq!(row["last_error"], "graphql 5xx");
        assert!(row.get("updated_at").is_some(), "updated_at key present");
    }

    #[test]
    fn dead_letter_json_empty_is_zero_count() {
        let v = dead_letter_json(&[]);
        assert_eq!(v["dead_lettered"], 0);
        assert_eq!(v["entries"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn parses_standard_issue_url() {
        assert_eq!(
            parse_issue_url("https://github.com/owner/repo/issues/123"),
            Some(("github.com/owner/repo".to_string(), "123".to_string()))
        );
    }

    #[test]
    fn parses_with_trailing_slash_and_http() {
        assert_eq!(
            parse_issue_url("http://github.com/o/r/issues/7/"),
            Some(("github.com/o/r".to_string(), "7".to_string()))
        );
    }

    #[test]
    fn rejects_non_issue_and_malformed_urls() {
        assert_eq!(parse_issue_url("not a url"), None);
        assert_eq!(parse_issue_url("https://github.com/o/r"), None); // no /issues/N
        assert_eq!(parse_issue_url("https://github.com/o/r/pull/1"), None); // PR, not issue
        assert_eq!(parse_issue_url("https://gitlab.com/o/r/issues/1"), None); // wrong host
        assert_eq!(parse_issue_url("https://github.com/o/r/issues/abc"), None); // non-numeric
        assert_eq!(
            parse_issue_url("https://github.com/o/r/issues/1/extra"),
            None
        ); // trailing
    }
}
