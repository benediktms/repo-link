//! `rl query` dispatch — the read-only workspace views.

use anyhow::{Result, anyhow};
use infra_config::RepoLinkConfig;

use crate::cli::{QueryCmd, WorkspaceArg};
use crate::commands::task::git_user_name;
use crate::render;
use crate::services::Services;

pub(crate) async fn query_dispatch(
    cmd: QueryCmd,
    svc: &Services,
    cfg: &RepoLinkConfig,
) -> Result<()> {
    match cmd {
        QueryCmd::Overview {
            ws: WorkspaceArg { workspace },
        } => {
            let v = svc.query.overview(&workspace).await?;
            render::overview(&v);
        }
        QueryCmd::Blocked {
            ws: WorkspaceArg { workspace },
        } => {
            let v = svc.query.blocked_tasks(&workspace).await?;
            render::blocked(&v);
        }
        QueryCmd::Stale {
            ws: WorkspaceArg { workspace },
        } => {
            let v = svc.query.stale_worktrees(&workspace).await?;
            render::stale(&v);
        }
        QueryCmd::Unsynced {
            ws: WorkspaceArg { workspace },
        } => {
            let v = svc.query.unsynced_tasks(&workspace).await?;
            render::unsynced(&v);
        }
        QueryCmd::Contributors {
            ws: WorkspaceArg { workspace },
        } => {
            let v = svc.query.contributors(&workspace).await?;
            render::contributors(&v);
        }
        QueryCmd::Drift {
            ws: WorkspaceArg { workspace },
        } => {
            let v = svc.query.drift(&workspace).await?;
            render::drift(&v);
        }
        QueryCmd::Ready {
            ws: WorkspaceArg { workspace },
        } => {
            let v = svc.query.ready_tasks(&workspace).await?;
            render::ready(&v);
        }
        QueryCmd::Mine {
            ws: WorkspaceArg { workspace },
            assignee,
        } => {
            let _ = cfg; // RepoLinkConfig is currently the env-var fallback chain.
            // Precedence: explicit --assignee > git config user.name >
            // REPO_LINK_USER > $USER. Git user comes ahead of env vars so
            // multi-repo dev setups where each repo has a different
            // committer identity stay accurate by default.
            let assignee = assignee
                .or_else(git_user_name)
                .or_else(|| std::env::var("REPO_LINK_USER").ok())
                .or_else(|| std::env::var("USER").ok())
                .ok_or_else(|| {
                    anyhow!("set --assignee, configure `git config user.name`, or set REPO_LINK_USER / USER")
                })?;
            let v = svc.query.assigned_to(&workspace, &assignee).await?;
            render::assigned(&v);
        }
    }
    Ok(())
}
